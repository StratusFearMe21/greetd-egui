use std::{
    borrow::Cow,
    ffi::c_void,
    io::{Read, Write},
    os::unix::{
        net::UnixStream,
        prelude::{AsRawFd, FromRawFd},
    },
    path::{Path, PathBuf},
    rc::Rc,
};

use clap::Arg;
use egui::{Align2, Frame, TextEdit};
use freedesktop_desktop_entry::DesktopEntry;
use greetd_ipc::{codec::SyncCodec, AuthMessageType, ErrorType, Response};

use glutin::{
    event::{DeviceId, ModifiersState, VirtualKeyCode},
    event_loop::ControlFlow,
    platform::{run_return::EventLoopExtRunReturn, unix::EventLoopWindowTargetExtUnix},
    window::Window,
    ContextWrapper, PossiblyCurrent,
};
use infer::MatcherType;
use libmpv::{
    render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType},
    FileState, Mpv,
};
use rand::prelude::IteratorRandom;

#[derive(PartialEq)]
struct StrippedEntry<'a> {
    name: Cow<'a, str>,
    exec: &'a str,
}

#[derive(Debug)]
enum UserEvent {
    Char(char),
    Redraw,
}

pub fn get_proc_address(
    display: &Rc<ContextWrapper<PossiblyCurrent, Window>>,
    name: &str,
) -> *mut c_void {
    display.get_proc_address(name) as *mut c_void
}

fn main() {
    let command = clap::Command::new("eguigreeter")
        .args(&[
            Arg::new("background")
                .long("background")
                .short('b')
                .value_hint(clap::ValueHint::AnyPath)
                .help("Sets the background picture on the lock screen"),
            Arg::new("username")
                .long("username")
                .short('u')
                .value_hint(clap::ValueHint::Username)
                .help("Autofill your own username on a single user computer"),
            Arg::new("session")
                .long("session")
                .short('s')
                .value_hint(clap::ValueHint::Other)
                .help("Sets the default session for this login"),
        ])
        .get_matches();
    let mut event_loop: glutin::event_loop::EventLoop<UserEvent> =
        glutin::event_loop::EventLoopBuilder::with_user_event().build();
    let display = unsafe {
        Rc::new(
            glutin::ContextBuilder::new()
                .with_vsync(true)
                .build_windowed(
                    glutin::window::WindowBuilder::new().with_resizable(true),
                    &event_loop,
                )
                .unwrap()
                .make_current()
                .unwrap(),
        )
    };
    let mut size = display.window().inner_size();

    let gl = unsafe {
        Rc::new(glow::Context::from_loader_function(|c| {
            display.get_proc_address(c)
        }))
    };

    let mut egui_glow = egui_glow::EguiGlow::new(display.window(), gl.clone());

    let mut vid = || -> Option<(Option<RenderContext>, Mpv)> {
        let mut path = command.value_of("background")?.to_string();

        if Path::new(&path).is_dir() {
            path = std::fs::read_dir(path)
                .ok()?
                .choose(&mut rand::rngs::OsRng)?
                .ok()?
                .path()
                .to_str()?
                .to_string();
        } else if !Path::new(&path).exists() {
            return None;
        }

        let is_image = if let Some(mime) = infer::Infer::new().get_from_path(&path).ok()? {
            mime.matcher_type() == MatcherType::Image
        } else {
            false
        };

        let mut mpv = Mpv::with_initializer(|f| {
            if is_image {
                f.set_property("keep-open", true)?;
            } else {
                f.set_property("audio", false)?;
                f.set_property("hwdec", "auto-safe")?;
                f.set_property("loop-file", true)?;
            }
            f.set_property("panscan", 1.0)
        })
        .ok()?;
        let mut render_context = RenderContext::new(
            unsafe { mpv.ctx.as_mut() },
            vec![
                RenderParam::ApiType(RenderParamApiType::OpenGl),
                RenderParam::InitParams(OpenGLInitParams {
                    get_proc_address,
                    ctx: display.clone(),
                }),
            ],
        )
        .ok()?;
        mpv.event_context_mut().disable_deprecated_events().unwrap();
        let event_proxy = event_loop.create_proxy();
        render_context.set_update_callback(move || {
            event_proxy.send_event(UserEvent::Redraw).unwrap();
        });
        mpv.playlist_load_files(&[(&path, FileState::AppendPlay, None)])
            .unwrap();

        Some((Some(render_context), mpv))
    }();

    let mut stream = UnixStream::connect(std::env::var("GREETD_SOCK").unwrap()).unwrap();

    let mut pending_message = false;
    let mut focused = FocusedField::Username;
    let mut username = String::new();

    if let Some(defaults) = command.value_of("username") {
        username = defaults.to_string();
        let len: u32 =
            ("{\"type\":\"create_session\",\"username\":\"\"}".len() + username.len()) as u32;
        stream.write_all(&len.to_ne_bytes()).unwrap();
        stream
            .write_fmt(format_args!(
                "{{\"type\":\"create_session\",\"username\":\"{}\"}}",
                username
            ))
            .unwrap();
        pending_message = true;
        focused = FocusedField::Password;
    }

    crossterm::terminal::enable_raw_mode().unwrap();

    std::thread::spawn({
        let proxy = event_loop.create_proxy();
        move || {
            let stdin = std::io::stdin();
            let mut lock = stdin.lock();
            let mut b = [0x00];
            loop {
                if let Err(_) = lock.read_exact(&mut b) {
                    crossterm::terminal::disable_raw_mode().unwrap();
                    std::process::exit(1);
                }
                proxy.send_event(UserEvent::Char(b[0] as char)).unwrap();
            }
        }
    });

    let environments_raw: Vec<(String, PathBuf)> = freedesktop_desktop_entry::Iter::new(vec![
        PathBuf::from("/usr/share/wayland-sessions"),
        PathBuf::from("/usr/share/xsessions"),
    ])
    .filter_map(|path| Some((std::fs::read_to_string(&path).ok()?, path)))
    .collect();
    let environments_serialized: Vec<DesktopEntry> = environments_raw
        .iter()
        .filter_map(|(bytes, path)| DesktopEntry::decode(&path, &bytes).ok())
        .collect();
    let environments: Vec<StrippedEntry> = environments_serialized
        .iter()
        .filter_map(|f| {
            Some(StrippedEntry {
                name: f.name(None)?,
                exec: f.exec()?,
            })
        })
        .collect();
    let mut current_env_index = if let Some(session) = command.value_of("session") {
        environments
            .iter()
            .position(|f| f.name == Cow::Borrowed(session))
            .unwrap_or(0)
    } else {
        0
    };
    let mut current_env = &environments[current_env_index];
    let mut pending_focus = true;
    let mut auth_message = String::new();
    let mut auth_message_type: Option<AuthMessageType> = None;
    let mut password = String::new();
    let mut window_title = Cow::Borrowed("Login");
    let mut card_fd = None;
    event_loop.run_return(|event, target, control_flow| {
        if pending_message {
            match Response::read_from(&mut stream).unwrap() {
                Response::AuthMessage {
                    auth_message_type: at,
                    auth_message: am,
                } => {
                    auth_message = am;
                    auth_message_type = Some(at);
                    pending_message = false;
                }
                Response::Success => {
                    if let Some(drm) = target.drm_device() {
                        card_fd = Some(drm.as_raw_fd());
                    }
                    *control_flow = ControlFlow::Exit;
                    pending_message = false;
                }
                Response::Error {
                    error_type,
                    description,
                } => match error_type {
                    ErrorType::Error => window_title = Cow::Owned(description),
                    ErrorType::AuthError => {
                        stream =
                            UnixStream::connect(std::env::var("GREETD_SOCK").unwrap()).unwrap();
                        focused = FocusedField::Username;
                        username.clear();
                        password.clear();

                        if let Some(defaults) = command.value_of("username") {
                            username = defaults.to_string();
                            let len: u32 = ("{\"type\":\"create_session\",\"username\":\"\"}".len()
                                + username.len()) as u32;
                            stream.write_all(&len.to_ne_bytes()).unwrap();
                            stream
                                .write_fmt(format_args!(
                                    "{{\"type\":\"create_session\",\"username\":\"{}\"}}",
                                    username
                                ))
                                .unwrap();
                            pending_message = true;
                            focused = FocusedField::Password;
                        } else {
                            pending_message = false;
                        }
                    }
                },
            }
        }
        match event {
            glutin::event::Event::LoopDestroyed => {
                egui_glow.destroy();
                if let Some(v) = &mut vid {
                    v.0.take();
                }
            }
            glutin::event::Event::RedrawRequested(_) => {
                let needs_repaint = egui_glow.run(display.window(), |ctx| {
                    egui::Window::new(window_title.as_ref())
                        .auto_sized()
                        .collapsible(false)
                        .anchor(Align2::CENTER_CENTER, (0.0, 0.0))
                        .show(ctx, |ui| {
                            ui.horizontal(|ui| {
                                ui.label("Username: ");
                                let resp = ui.text_edit_singleline(&mut username);
                                if pending_focus {
                                    if let FocusedField::Username = focused {
                                        ui.memory().request_focus(resp.id);
                                        pending_focus = false;
                                    }
                                }
                            });

                            ui.horizontal(|ui| {
                                if auth_message_type.is_some() {
                                    ui.label(&auth_message);
                                } else {
                                    return;
                                }
                                let resp = match auth_message_type {
                                    Some(AuthMessageType::Visible) => {
                                        ui.add(TextEdit::singleline(&mut password))
                                    }
                                    Some(AuthMessageType::Secret) => {
                                        ui.add(TextEdit::singleline(&mut password).password(true))
                                    }
                                    Some(_) => {
                                        stream
                                            .write_all(
                                                &("{\"type\":\"post_auth_message_response\"}".len()
                                                    as u32)
                                                    .to_ne_bytes(),
                                            )
                                            .unwrap();
                                        stream
                                            .write_all(b"{\"type\":\"post_auth_message_response\"}")
                                            .unwrap();
                                        pending_message = true;
                                        return;
                                    }
                                    None => return,
                                };
                                if pending_focus {
                                    if let FocusedField::Password = focused {
                                        ui.memory().request_focus(resp.id);
                                        pending_focus = false;
                                    }
                                }
                            });

                            ui.label(format!("Session: < {} >", current_env.name));
                        });
                });

                *control_flow = if needs_repaint {
                    display.window().request_redraw();
                    ControlFlow::Poll
                } else if *control_flow != ControlFlow::Exit {
                    ControlFlow::Wait
                } else {
                    ControlFlow::Exit
                };

                {
                    unsafe {
                        use glow::HasContext as _;
                        gl.clear_color(0.0, 0.0, 0.0, 1.0);
                        gl.clear(glow::COLOR_BUFFER_BIT);
                    }

                    if let Some(vi) = &vid {
                        if let Some(render_context) = &vi.0 {
                            render_context
                                .render::<ContextWrapper<PossiblyCurrent, Window>>(
                                    0,
                                    size.width as _,
                                    size.height as _,
                                    true,
                                )
                                .expect("Failed to draw on glutin window");
                        }
                    }

                    egui_glow.paint(display.window());

                    display.swap_buffers().unwrap();
                }
            }
            glutin::event::Event::UserEvent(c) => {
                match c {
                    UserEvent::Char('\r') => match focused {
                        FocusedField::Password => {
                            if username.is_empty() {
                                focused = FocusedField::Username;
                            } else {
                                stream
                                    .write_all(
                                        &(("{\"type\":\"post_auth_message_response\",\"\
                                                    response\":\"\"}"
                                            .len()
                                            + password.len())
                                            as u32)
                                            .to_ne_bytes(),
                                    )
                                    .unwrap();
                                stream
                                    .write_fmt(format_args!(
                                        "{{\"type\":\"post_auth_message_response\",\"\
                                                 response\":\"{}\"}}",
                                        password
                                    ))
                                    .unwrap();
                                pending_message = true;
                            }
                            pending_focus = true;
                        }
                        FocusedField::Username => {
                            let len: u32 = ("{\"type\":\"create_session\",\"username\":\"\"}".len()
                                + username.len()) as u32;
                            stream.write_all(&len.to_ne_bytes()).unwrap();
                            stream
                                .write_fmt(format_args!(
                                    "{{\"type\":\"create_session\",\"username\":\"{}\"}}",
                                    username
                                ))
                                .unwrap();
                            focused = FocusedField::Password;
                            pending_message = true;
                            pending_focus = true;
                        }
                    },
                    UserEvent::Char('\t') => {
                        match focused {
                            FocusedField::Username => focused = FocusedField::Password,
                            FocusedField::Password => focused = FocusedField::Username,
                        }
                        pending_focus = true;
                    }
                    UserEvent::Char('>') => {
                        current_env_index += 1;
                        if let Some(env) = environments.get(current_env_index) {
                            current_env = env
                        } else {
                            current_env_index = 0;
                            current_env = &environments[current_env_index];
                        }
                    }
                    UserEvent::Char('<') => {
                        current_env_index -= 1;
                        if let Some(env) = environments.get(current_env_index) {
                            current_env = env
                        } else {
                            current_env_index = environments.len() - 1;
                            current_env = &environments[current_env_index];
                        }
                    }
                    UserEvent::Char('\x7F') => {
                        egui_glow.on_event(&glutin::event::WindowEvent::KeyboardInput {
                            device_id: unsafe { DeviceId::dummy() },
                            input: glutin::event::KeyboardInput {
                                scancode: b'\x7F' as u32,
                                state: glutin::event::ElementState::Pressed,
                                virtual_keycode: Some(VirtualKeyCode::Back),
                                modifiers: ModifiersState::empty(),
                            },
                            is_synthetic: false,
                        });
                        egui_glow.on_event(&glutin::event::WindowEvent::KeyboardInput {
                            device_id: unsafe { DeviceId::dummy() },
                            input: glutin::event::KeyboardInput {
                                scancode: b'\x7F' as u32,
                                state: glutin::event::ElementState::Released,
                                virtual_keycode: Some(VirtualKeyCode::Back),
                                modifiers: ModifiersState::empty(),
                            },
                            is_synthetic: false,
                        });
                    }
                    UserEvent::Char(c) => {
                        egui_glow.on_event(&glutin::event::WindowEvent::ReceivedCharacter(c));
                    }
                    UserEvent::Redraw => {}
                }

                display.window().request_redraw();
            }
            glutin::event::Event::WindowEvent { event, .. } => {
                use glutin::event::WindowEvent;
                if matches!(event, WindowEvent::CloseRequested | WindowEvent::Destroyed) {
                    *control_flow = glutin::event_loop::ControlFlow::Exit;
                }

                if let glutin::event::WindowEvent::Resized(physical_size) = &event {
                    size = *physical_size;
                    display.resize(*physical_size);
                } else if let glutin::event::WindowEvent::ScaleFactorChanged {
                    new_inner_size,
                    ..
                } = &event
                {
                    size = **new_inner_size;
                    display.resize(**new_inner_size);
                }

                egui_glow.on_event(&event);

                display.window().request_redraw();
            }
            _ => {}
        }
    });
    drop(event_loop);
    drop(display);
    drop(egui_glow);
    if let Some(fd) = card_fd {
        drop(unsafe { std::fs::File::from_raw_fd(fd) });
    }
    stream
        .write_all(
            &(("{\"type\":\"start_session\",\"cmd\":[\"/etc/ly/wsetup.sh\",\"\"]}".len()
                + current_env.exec.len()) as u32)
                .to_ne_bytes(),
        )
        .unwrap();
    stream
        .write_fmt(format_args!(
            "{{\"type\":\"start_session\",\"cmd\":[\"/etc/ly/wsetup.sh\",\"{}\"]}}",
            current_env.exec
        ))
        .unwrap();
    if let Response::Success = Response::read_from(&mut stream).unwrap() {
        return;
    }
}

#[derive(PartialEq, Eq)]
enum FocusedField {
    Username,
    Password,
}
