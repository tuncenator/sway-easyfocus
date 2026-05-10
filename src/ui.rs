use std::{cell::RefCell, collections::HashMap, fmt, rc::Rc, time::Duration};

use gtk4::{
    Application, CssProvider,
    gdk::prelude::{DisplayExt, MonitorExt},
    gio::prelude::{ApplicationExt, ApplicationExtManual, ListModelExt},
    glib::{self, ControlFlow, Propagation, object::Cast},
    prelude::{DrawingAreaExtManual, FixedExt, GtkWindowExt, WidgetExt},
};
use pango::prelude::FontMapExt;

use crate::{cli::Command, options::Options, sway};

fn calculate_geometry(
    window: &swayipc::Node,
    output: &swayipc::Node,
    opts: &Options,
) -> (i32, i32) {
    let rect = window.rect;
    let window_rect = window.window_rect;

    let anchor_x = output.rect.x;
    let anchor_y = output.rect.y;

    let rel_x = rect.x + window_rect.x + opts.label_margin_x;
    let rel_y = rect.y + window_rect.y + opts.label_margin_y;

    (rel_x - anchor_x, rel_y - anchor_y)
}

fn handle_keypress(
    conn: &mut swayipc::Connection,
    keys_to_con_ids: &HashMap<char, i64>,
    keyval: &str,
    command: Command,
) -> Result<Option<char>, swayipc::Error> {
    if keyval.len() == 1 {
        // we can unwrap because keyval has length 1
        let c = keyval.chars().next().unwrap();
        if c.is_alphanumeric() {
            if let Some(&con_id) = keys_to_con_ids.get(&c) {
                return match command {
                    Command::Focus => {
                        sway::focus(conn, con_id)?;
                        Ok(Some(c))
                    }
                    Command::Swap { focus } => {
                        sway::swap(conn, con_id)?;

                        if focus {
                            sway::focus(conn, con_id)?;
                        }
                        Ok(Some(c))
                    }
                    Command::Print => {
                        println!("{}", con_id);
                        Ok(Some(c))
                    }
                }
            }
        }
    }
    Ok(None)
}

fn handle_confirmation(windows: &[gtk4::ApplicationWindow], c: char) {
    for window in windows {
        if let Some(fixed) = window
            .child()
            .and_then(|c| c.downcast::<gtk4::Fixed>().ok())
        {
            let mut child = fixed.first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();

                if let Ok(label) = widget.downcast::<gtk4::Label>() {
                    if label.text() == c.to_string() {
                        label.add_css_class("focused");
                    } else {
                        label.set_visible(false);
                    }
                }
            }
        }
    }
}

fn create_key_controller(
    conn: Rc<RefCell<swayipc::Connection>>,
    windows: Rc<Vec<gtk4::ApplicationWindow>>,
    keys_to_con_ids: Rc<HashMap<char, i64>>,
    opts: Rc<Options>,
) -> gtk4::EventControllerKey {
    let key_controller = gtk4::EventControllerKey::new();
    key_controller.connect_key_pressed(move |_, keyval, _keycode, _state| {
        if let Some(keyval) = keyval.name() {
            let mut delay = 0;

            match handle_keypress(
                &mut conn.borrow_mut(),
                &keys_to_con_ids,
                keyval.as_str(),
                opts.command,
            ) {
                Err(e) => eprintln!("{}", e),
                Ok(Some(c)) => {
                    if opts.show_confirmation {
                        delay = 500;
                        handle_confirmation(windows.as_ref(), c);
                    }
                }
                _ => {}
            };

            let windows = windows.clone();
            glib::timeout_add_local(Duration::from_millis(delay), move || {
                for window in windows.as_ref() {
                    window.close();
                }
                ControlFlow::Break
            });

            Propagation::Stop
        } else {
            Propagation::Proceed
        }
    });

    key_controller
}

fn build_ui(
    app: &Application,
    conn: &Rc<RefCell<swayipc::Connection>>,
    opts: &Rc<Options>,
) -> Result<(), Error> {
    let tree = conn
        .try_borrow_mut()
        .map_err(|_| Error::ConnectionError)?
        .get_tree()?;

    let outputs = sway::parse_output_nodes(&tree);
    let mut chars = opts.chars.chars();

    let mut keys_to_con_ids = HashMap::new();
    let mut windows = Vec::new();

    for output in outputs {
        let window = gtk4::ApplicationWindow::new(app);

        gtk4_layer_shell::LayerShell::init_layer_shell(&window);
        gtk4_layer_shell::LayerShell::set_namespace(&window, Some("sway-easyfocus"));
        gtk4_layer_shell::LayerShell::set_layer(&window, gtk4_layer_shell::Layer::Overlay);
        gtk4_layer_shell::LayerShell::set_keyboard_mode(
            &window,
            gtk4_layer_shell::KeyboardMode::Exclusive,
        );
        gtk4_layer_shell::LayerShell::set_anchor(&window, gtk4_layer_shell::Edge::Top, true);
        gtk4_layer_shell::LayerShell::set_anchor(&window, gtk4_layer_shell::Edge::Bottom, true);
        gtk4_layer_shell::LayerShell::set_anchor(&window, gtk4_layer_shell::Edge::Left, true);
        gtk4_layer_shell::LayerShell::set_anchor(&window, gtk4_layer_shell::Edge::Right, true);
        // Cover the full output, ignoring exclusive zones reserved by bars
        // and other layer-shell clients. Without this, the overlay's origin
        // is shifted by the bar height and labels misalign with windows.
        gtk4_layer_shell::LayerShell::set_exclusive_zone(&window, -1);

        let display = gtk4::gdk::Display::default().unwrap();
        let monitors = display.monitors();
        for i in 0..monitors.n_items() {
            if let Some(monitor) = monitors
                .item(i)
                .and_then(|obj| obj.downcast::<gtk4::gdk::Monitor>().ok())
            {
                let geometry = monitor.geometry();
                if geometry.x() <= output.rect.x
                    && output.rect.x < geometry.x() + geometry.width()
                    && geometry.y() <= output.rect.y
                    && output.rect.y < geometry.y() + geometry.height()
                {
                    gtk4_layer_shell::LayerShell::set_monitor(&window, Some(&monitor));
                    break;
                }
            }
        }

        let fixed = gtk4::Fixed::new();

        if let Some(workspace) = sway::find_focused_workspace(output) {
            let client_windows = sway::get_all_windows(&workspace);

            // Build a Pango font description so DrawingArea labels can be
            // sized and rendered to a tight ink-rect bounding box. Default
            // font_size to 14 if unparseable as <N>px.
            let font_px: i32 = opts
                .font_size
                .strip_suffix("px")
                .and_then(|s| s.parse().ok())
                .unwrap_or(14);
            let font_desc_str =
                format!("{} {} {}", opts.font_family, opts.font_weight, font_px);
            let base_font_desc = pango::FontDescription::from_string(&font_desc_str);

            // Create labels for windows
            for client in client_windows.iter() {
                let (x, y) = calculate_geometry(client, &output, opts);

                let letter = chars.next().ok_or(Error::OutOfCharsError)?;
                let letter_str = letter.to_string();

                // Compute the glyph's ink rect using a temporary layout so we
                // can size the DrawingArea exactly to the visible glyph.
                let temp_ctx = pangocairo::FontMap::default().create_context();
                let temp_layout = pango::Layout::new(&temp_ctx);
                temp_layout.set_text(&letter_str);
                temp_layout.set_font_description(Some(&base_font_desc));
                let (ink, logical) = temp_layout.pixel_extents();
                let pad_x = opts.label_padding_x;
                let pad_y = opts.label_padding_y;
                // Use logical.width (advance) so monospace glyphs share a
                // uniform cell width. Use ink.height for a tight vertical
                // fit so the box hugs the glyph top and bottom.
                let area_w = logical.width().max(1) + 2 * pad_x;
                let area_h = ink.height().max(1) + 2 * pad_y;
                let logical_x = logical.x();
                let ink_y = ink.y();

                let (bg, bg_a, fg) = if client.focused {
                    (
                        opts.focused_background_color,
                        opts.focused_background_opacity,
                        opts.focused_text_color,
                    )
                } else {
                    (
                        opts.label_background_color,
                        opts.label_background_opacity,
                        opts.label_text_color,
                    )
                };

                let area = gtk4::DrawingArea::new();
                area.set_size_request(area_w, area_h);

                let fd = base_font_desc.clone();
                let letter_for_draw = letter_str.clone();
                area.set_draw_func(move |_, cr, w, h| {
                    cr.set_source_rgba(
                        bg.r as f64 / 255.0,
                        bg.g as f64 / 255.0,
                        bg.b as f64 / 255.0,
                        bg_a,
                    );
                    cr.rectangle(0.0, 0.0, w as f64, h as f64);
                    let _ = cr.fill();

                    cr.set_source_rgba(
                        fg.r as f64 / 255.0,
                        fg.g as f64 / 255.0,
                        fg.b as f64 / 255.0,
                        1.0,
                    );
                    let layout = pangocairo::functions::create_layout(cr);
                    layout.set_text(&letter_for_draw);
                    layout.set_font_description(Some(&fd));
                    cr.move_to(
                        (pad_x - logical_x) as f64,
                        (pad_y - ink_y) as f64,
                    );
                    pangocairo::functions::show_layout(cr, &layout);
                });

                fixed.put(&area, x as f64, y as f64);

                keys_to_con_ids.insert(letter, client.id);
            }
        }

        window.set_child(Some(&fixed));
        windows.push(window);
    }

    if !keys_to_con_ids.is_empty() {
        let keys_to_con_ids = Rc::new(keys_to_con_ids);
        let windows = Rc::new(windows);

        for window in windows.iter() {
            window.add_controller(create_key_controller(
                conn.clone(),
                windows.clone(),
                keys_to_con_ids.clone(),
                opts.clone(),
            ));
            window.present();
        }
    }

    Ok(())
}

fn load_css(opts: &Options) {
    let provider = CssProvider::new();
    provider.load_from_data(&opts.to_css());

    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().unwrap(),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

pub fn run_ui(conn: swayipc::Connection, opts: Rc<Options>) {
    let app = Application::builder()
        .application_id("com.github.edzdez.sway-easyfocus")
        .build();

    let opts_clone = opts.clone();
    app.connect_startup(move |_| load_css(&opts_clone));

    let conn = Rc::new(RefCell::new(conn));
    app.connect_activate(move |app| {
        if let Err(err) = build_ui(app, &conn, &opts) {
            eprintln!("{}", err);
        }
    });

    app.run_with_args::<String>(&[]);
}

#[derive(Debug)]
pub enum Error {
    ConnectionError,
    SwayIpcError(swayipc::Error),
    OutOfCharsError,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionError => f.write_str("An error occured with the connection."),
            Self::SwayIpcError(err) => f.write_fmt(format_args!("{}", err)),
            Self::OutOfCharsError => f.write_str("Ran out of character labels."),
        }
    }
}

impl From<swayipc::Error> for Error {
    fn from(value: swayipc::Error) -> Self {
        Self::SwayIpcError(value)
    }
}
