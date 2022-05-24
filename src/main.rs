use std::{
    borrow::Cow,
    cell::RefCell,
    ffi::c_void,
    io::Read,
    os::unix::prelude::FromRawFd,
    path::{Path, PathBuf},
    rc::Rc,
};

use calloop::{Interest, PostAction};
use clap::Arg;
use egui::{Align2, Color32, RichText, TextEdit};
use freedesktop_desktop_entry::DesktopEntry;
use greetd_client::{AuthMessageType, ErrorType, Greetd, GreetdSource, Response};

use glutin::{
    event::{DeviceId, ModifiersState, VirtualKeyCode},
    event_loop::ControlFlow,
    platform::{
        run_return::EventLoopExtRunReturn,
        unix::{EventLoopWindowTargetExtUnix, WindowExtUnix},
    },
    window::{Window, WindowId},
    ContextWrapper, PossiblyCurrent,
};
use infer::MatcherType;
use libmpv::{
    render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType},
    FileState, Mpv,
};
use rand::prelude::IteratorRandom;
use time::{
    format_description::modifier::{Hour, Minute},
    UtcOffset,
};
use tz::TimeZone;

#[derive(PartialEq)]
struct StrippedEntry<'a> {
    name: Cow<'a, str>,
    exec: &'a str,
}

#[derive(Debug)]
enum UserEvent {
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
                f.set_property("loop-file", true)?;
                f.set_property("hwdec", "auto-safe")?;
            }
            f.set_property("panscan", 1.0)
        })
        .ok()?;
        if Path::new("/etc/mpv/mpv.conf").exists() {
            mpv.load_config("/etc/mpv/mpv.conf").ok()?;
        }
        let mut params = vec![
            RenderParam::ApiType(RenderParamApiType::OpenGl),
            RenderParam::InitParams(OpenGLInitParams {
                get_proc_address,
                ctx: display.clone(),
            }),
        ];
        if let Some(display) = event_loop.wayland_display() {
            params.push(RenderParam::WaylandDisplay(display as _));
        } else if let Some(display) = display.window().xlib_display() {
            params.push(RenderParam::X11Display(display as _));
        }
        let mut render_context = RenderContext::new(unsafe { mpv.ctx.as_mut() }, params).ok()?;
        mpv.event_context_mut().disable_deprecated_events().unwrap();
        let event_proxy = event_loop.create_proxy();
        render_context.set_update_callback(move || {
            event_proxy.send_event(UserEvent::Redraw).unwrap();
        });
        mpv.playlist_load_files(&[(&path, FileState::AppendPlay, None)])
            .unwrap();

        Some((Some(render_context), mpv))
    }();

    let mut stream = Greetd::new().unwrap();
    let response_queue = Rc::new(RefCell::new(None));

    let mut focused = FocusedField::Username;
    let mut username = String::new();

    if let Some(defaults) = command.value_of("username") {
        username = defaults.to_string();
        stream.create_session(&username).unwrap();
        focused = FocusedField::Password;
    }

    crossterm::terminal::enable_raw_mode().unwrap();

    let timezone = TimeZone::local().unwrap();
    let offset = timezone.find_current_local_time_type().unwrap().ut_offset();
    let current_time =
        time::OffsetDateTime::now_utc().to_offset(UtcOffset::from_whole_seconds(offset).unwrap());
    let clock = current_time
        .format(
            [
                time::format_description::FormatItem::Component(
                    time::format_description::Component::Hour({
                        let mut h = Hour::default();
                        h.is_12_hour_clock = true;
                        h
                    }),
                ),
                time::format_description::FormatItem::Literal(b":"),
                time::format_description::FormatItem::Component(
                    time::format_description::Component::Minute(Minute::default()),
                ),
            ]
            .as_ref(),
        )
        .unwrap_or_else(|_| "??:??".to_string());

    if let Some(handle) = event_loop.drm_calloop_handle() {
        let stdin_source = calloop::generic::Generic::new(
            unsafe { std::fs::File::from_raw_fd(0) },
            Interest::READ,
            calloop::Mode::Level,
        );

        let stdin_dispatcher: calloop::Dispatcher<
            'static,
            calloop::generic::Generic<std::fs::File>,
            Vec<glutin::event::Event<'static, ()>>,
        > = calloop::Dispatcher::new(
            stdin_source,
            move |_, stdin, shared_data: &mut Vec<glutin::event::Event<'static, ()>>| {
                let mut b = [0x00];
                if let Err(_) = stdin.read_exact(&mut b) {
                    crossterm::terminal::disable_raw_mode().unwrap();
                    return Ok(PostAction::Remove);
                }
                shared_data.push(glutin::event::Event::WindowEvent {
                    window_id: unsafe { WindowId::dummy() },
                    event: glutin::event::WindowEvent::ReceivedCharacter(b[0] as char),
                });
                Ok(PostAction::Continue)
            },
        );

        handle.register_dispatcher(stdin_dispatcher).unwrap();

        let rq = response_queue.clone();

        let stream_dispatcher: calloop::Dispatcher<
            'static,
            GreetdSource,
            Vec<glutin::event::Event<'static, ()>>,
        > = calloop::Dispatcher::new(stream.event_source(), move |event, _, _| {
            let mut rs = rq.borrow_mut();
            if rs.is_some() {
                panic!("Multiple events cannot be in the queue at once");
            } else {
                *rs = Some(event);
            }
        });

        handle.register_dispatcher(stream_dispatcher).unwrap();
    }

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
    event_loop.run_return(|event, _, control_flow| {
        if let Some(i) = response_queue.take() {
            match i {
                Response::AuthMessage {
                    auth_message_type: at,
                    auth_message: am,
                } => {
                    auth_message = am;
                    auth_message_type = Some(at);
                    if let Some(AuthMessageType::Info) | Some(AuthMessageType::Error) =
                        auth_message_type
                    {
                        stream.authentication_response(None).unwrap();
                    }
                    display.window().request_redraw();
                }
                Response::Finish => {
                    *control_flow = ControlFlow::Exit;
                }
                Response::Success => {
                    stream
                        .start_session(&["/etc/ly/wsetup.sh", current_env.exec])
                        .unwrap();
                }
                Response::Error {
                    error_type,
                    description,
                } => {
                    match error_type {
                        ErrorType::Error => window_title = Cow::Owned(description),
                        ErrorType::AuthError => {
                            window_title = Cow::Borrowed("Login failed");
                            focused = FocusedField::Username;
                            pending_focus = true;
                            auth_message_type = None;
                            username.clear();
                            password.clear();

                            if let Some(defaults) = command.value_of("username") {
                                username = defaults.to_string();
                                stream.create_session(&username).unwrap();
                                focused = FocusedField::Password;
                            }
                        }
                    }
                    display.window().request_redraw();
                }
            }
        }
        match event {
            glutin::event::Event::LoopDestroyed => {
                crossterm::terminal::disable_raw_mode().unwrap();
                egui_glow.destroy();
                if let Some(v) = &mut vid {
                    v.0.take();
                }
                vid.take();
            }
            glutin::event::Event::RedrawRequested(_) => {
                let needs_repaint = egui_glow.run(display.window(), |ctx| {
                    egui::Window::new("")
                        .title_bar(false)
                        .auto_sized()
                        .collapsible(false)
                        .anchor(Align2::RIGHT_TOP, (-5.0, 5.0))
                        .show(ctx, |ui| {
                            ui.add(egui::Label::new(
                                RichText::new(&clock).size(48.0).color(Color32::WHITE),
                            ));
                        });
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
                                    _ => return,
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
            glutin::event::Event::UserEvent(_) => {
                display.window().request_redraw();
            }
            glutin::event::Event::WindowEvent { event, .. } => {
                use glutin::event::WindowEvent;
                if matches!(event, WindowEvent::CloseRequested | WindowEvent::Destroyed) {
                    *control_flow = glutin::event_loop::ControlFlow::Exit;

                    egui_glow.on_event(&event);
                }

                if let glutin::event::WindowEvent::Resized(physical_size) = &event {
                    size = *physical_size;
                    display.resize(*physical_size);
                    egui_glow.on_event(&event);
                } else if let glutin::event::WindowEvent::ScaleFactorChanged {
                    new_inner_size,
                    ..
                } = &event
                {
                    size = **new_inner_size;
                    display.resize(**new_inner_size);
                    egui_glow.on_event(&event);
                } else if let glutin::event::WindowEvent::ReceivedCharacter(c) = event {
                    match c {
                        '\r' => match focused {
                            FocusedField::Password => {
                                if username.is_empty() {
                                    focused = FocusedField::Username;
                                } else {
                                    stream.authentication_response(Some(&password)).unwrap();
                                }
                                pending_focus = true;
                            }
                            FocusedField::Username => {
                                stream.create_session(&username).unwrap();
                                focused = FocusedField::Password;
                                pending_focus = true;
                            }
                        },
                        '\t' => {
                            match focused {
                                FocusedField::Username => focused = FocusedField::Password,
                                FocusedField::Password => focused = FocusedField::Username,
                            }
                            pending_focus = true;
                        }
                        '>' => {
                            current_env_index += 1;
                            if let Some(env) = environments.get(current_env_index) {
                                current_env = env
                            } else {
                                current_env_index = 0;
                                current_env = &environments[current_env_index];
                            }
                        }
                        '<' => {
                            current_env_index -= 1;
                            if let Some(env) = environments.get(current_env_index) {
                                current_env = env
                            } else {
                                current_env_index = environments.len() - 1;
                                current_env = &environments[current_env_index];
                            }
                        }
                        '\x7F' => {
                            #[allow(deprecated)]
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
                            #[allow(deprecated)]
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
                        c => {
                            egui_glow.on_event(&glutin::event::WindowEvent::ReceivedCharacter(c));
                        }
                    }
                } else {
                    egui_glow.on_event(&event);
                }
                display.window().request_redraw();
            }
            _ => {}
        }
    });
}

#[derive(PartialEq, Eq)]
enum FocusedField {
    Username,
    Password,
}
