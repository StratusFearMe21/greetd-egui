use std::{
    borrow::Cow,
    io::{Read, Write},
    os::unix::{
        net::UnixStream,
        prelude::{AsRawFd, FromRawFd},
    },
    path::{Path, PathBuf},
};

use clap::Arg;
use egui::{Align2, TextEdit};
use freedesktop_desktop_entry::DesktopEntry;
use greetd_ipc::{codec::SyncCodec, AuthMessageType, Response};

use glium::glutin::{
    self,
    event::{DeviceId, ModifiersState, VirtualKeyCode},
    event_loop::ControlFlow,
    platform::{run_return::EventLoopExtRunReturn, unix::EventLoopWindowTargetExtUnix},
};
use rand::prelude::IteratorRandom;

#[derive(PartialEq)]
struct StrippedEntry<'a> {
    name: Cow<'a, str>,
    exec: &'a str,
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
        ])
        .get_matches();
    let mut event_loop: glutin::event_loop::EventLoop<char> =
        glutin::event_loop::EventLoopBuilder::with_user_event().build();
    let context_builder = glutin::ContextBuilder::new().with_vsync(true);

    let display = glium::Display::new(
        glutin::window::WindowBuilder::new(),
        context_builder,
        &event_loop,
    )
    .unwrap();

    let mut egui_glium = egui_glium::EguiGlium::new(&display);

    let img = || -> Option<_> {
        let size = display.gl_window().window().inner_size();
        let path = Path::new(
            command
                .value_of("background")
                .unwrap_or("/etc/greetd/background.png"),
        );
        let image = if path.is_dir() {
            image::open(
                std::fs::read_dir(path)
                    .ok()?
                    .choose(&mut rand::rngs::OsRng)?
                    .ok()?
                    .path(),
            )
            .ok()?
        } else {
            image::open(path).ok()?
        }
        .resize_exact(
            size.width,
            size.height,
            image::imageops::FilterType::Lanczos3,
        )
        .to_rgba8();

        let image_dimensions = image.dimensions();
        let pixels: Vec<_> = image
            .into_vec()
            .chunks_exact(4)
            .map(|p| egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]))
            .flat_map(|color| color.to_array())
            .collect();

        let texture = glium::texture::RawImage2d::from_raw_rgba(pixels, image_dimensions);
        let egui_dimensions = egui::Vec2::new(texture.width as f32, texture.height as f32);
        let glium_texture =
            glium::texture::srgb_texture2d::SrgbTexture2d::new(&display, texture).ok()?;
        let glium_texture = std::rc::Rc::new(glium_texture);

        Some((
            egui_glium.painter.register_native_texture(glium_texture),
            egui_dimensions,
        ))
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
                proxy.send_event(b[0] as char).unwrap();
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
    let mut current_env = &environments[0];
    let mut pending_focus = true;
    let mut auth_message = String::new();
    let mut auth_message_type: Option<AuthMessageType> = None;
    let mut password = String::new();
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
                }
                Response::Success => {
                    if let Some(drm) = target.drm_device() {
                        card_fd = Some(drm.as_raw_fd());
                    }
                    *control_flow = ControlFlow::Exit;
                }
                r => unimplemented!("{:?}", r),
            }
            pending_message = false;
        }
        match event {
            glutin::event::Event::RedrawRequested(_) => {
                let needs_repaint = egui_glium.run(&display, |ctx| {
                    if let Some(img) = img {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            ui.centered_and_justified(|ui| {
                                ui.image(img.0, img.1);
                            });
                        });
                    }
                    egui::Window::new("Login")
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

                            egui::ComboBox::from_label("Session")
                                .selected_text(current_env.name.as_ref())
                                .show_ui(ui, |ui| {
                                    for i in &environments {
                                        ui.selectable_value(&mut current_env, i, i.name.as_ref());
                                    }
                                });
                        });
                });

                *control_flow = if needs_repaint {
                    display.gl_window().window().request_redraw();
                    ControlFlow::Poll
                } else if *control_flow != ControlFlow::Exit {
                    ControlFlow::Wait
                } else {
                    ControlFlow::Exit
                };

                {
                    use glium::Surface as _;
                    let mut target = display.draw();

                    if img.is_none() {
                        let color = egui::Rgba::from_rgb(0.1, 0.3, 0.2);
                        target.clear_color(color[0], color[1], color[2], color[3]);
                    }

                    egui_glium.paint(&display, &mut target);

                    target.finish().unwrap();
                }
            }
            glutin::event::Event::UserEvent(c) => {
                match c {
                    '\r' => match focused {
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
                    '\t' => {
                        match focused {
                            FocusedField::Username => focused = FocusedField::Password,
                            FocusedField::Password => focused = FocusedField::Username,
                        }
                        pending_focus = true;
                    }
                    '\x7F' => {
                        egui_glium.on_event(&glutin::event::WindowEvent::KeyboardInput {
                            device_id: unsafe { DeviceId::dummy() },
                            input: glutin::event::KeyboardInput {
                                scancode: b'\x7F' as u32,
                                state: glutin::event::ElementState::Pressed,
                                virtual_keycode: Some(VirtualKeyCode::Back),
                                modifiers: ModifiersState::empty(),
                            },
                            is_synthetic: false,
                        });
                        egui_glium.on_event(&glutin::event::WindowEvent::KeyboardInput {
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
                    _ => {}
                }
                egui_glium.on_event(&glutin::event::WindowEvent::ReceivedCharacter(c));

                display.gl_window().window().request_redraw();
            }
            glutin::event::Event::WindowEvent { event, .. } => {
                use glutin::event::WindowEvent;
                if matches!(event, WindowEvent::CloseRequested | WindowEvent::Destroyed) {
                    *control_flow = glutin::event_loop::ControlFlow::Exit;
                }

                egui_glium.on_event(&event);

                display.gl_window().window().request_redraw();
            }
            _ => {}
        }
    });
    drop(event_loop);
    drop(display);
    drop(egui_glium);
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
