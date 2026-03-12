use gtk::prelude::*;
use gtk::{
    Adjustment, FileChooserAction, FileChooserDialog, ResponseType,
    Window, WindowType, Box, Orientation, Button, Label, 
    ScrolledWindow, Toolbar, ToolButton, Settings, DrawingArea
};
use gdk::EventMask;
use glib; 
use std::path::PathBuf;
use std::fs::File;
use std::io::BufReader;
use rodio::{Decoder, OutputStream, Sink, OutputStreamHandle};
use std::sync::{Arc, Mutex};

#[allow(dead_code)]
pub fn run() {
    if gtk::init().is_err() { return; }

    let window = create_main_window();
    window.show_all();
    gtk::main();
}

/// Construit la fenêtre principale (interface DAW) sans lancer gtk::main().
/// Utile pour l'intégrer à un écran d'accueil déjà initialisé.
pub fn create_main_window() -> Window {
    let settings = Settings::get_default().unwrap();
    settings.set_property_gtk_application_prefer_dark_theme(true);

    let window = Window::new(WindowType::Toplevel);
    window.set_title("MixRust - Professional DAW");
    window.set_default_size(1100, 700);

    let audio_data = OutputStream::try_default().ok();
    let (_stream, handle) = match audio_data {
        Some((s, h)) => (Some(s), Some(Arc::new(h))),
        None => (None, None),
    };

    // Conserve le stream pour éviter qu'il soit libéré pendant la durée de vie de la fenêtre.
    if let Some(stream) = _stream {
        // Stocke le stream sur la fenêtre pour garder la sortie audio vivante.
        unsafe { window.set_data("mixrust_output_stream", stream); }
    }

    let main_vbox = Box::new(Orientation::Vertical, 0);
    window.add(&main_vbox);

    let toolbar = Toolbar::new();
    let add_track_btn = ToolButton::new::<Button>(None, Some("Add Track"));
    let play_all_btn = ToolButton::new::<Button>(None, Some("PLAY ALL"));
    let mute_all_btn = ToolButton::new::<Button>(None, Some("MUTE ALL")); // Bouton Mute All
    
    toolbar.insert(&add_track_btn, -1);
    toolbar.insert(&play_all_btn, -1);
    toolbar.insert(&mute_all_btn, -1);
    main_vbox.pack_start(&toolbar, false, false, 0);

    let all_sinks: Arc<Mutex<Vec<Arc<Mutex<Sink>>>>> = Arc::new(Mutex::new(Vec::new()));
    let scroll = ScrolledWindow::new(None::<&Adjustment>, None::<&Adjustment>);
    let track_container = Box::new(Orientation::Vertical, 5);
    scroll.add(&track_container);
    main_vbox.pack_start(&scroll, true, true, 0);

    // LOGIQUE PLAY ALL
    let sinks_for_play = Arc::clone(&all_sinks);
    play_all_btn.connect_clicked(move |_| {
        for s in sinks_for_play.lock().unwrap().iter() { 
            s.lock().unwrap().play(); 
        }
    });

    // LOGIQUE MUTE ALL
    let sinks_for_mute_all = Arc::clone(&all_sinks);
    mute_all_btn.connect_clicked(move |_| {
        let sinks = sinks_for_mute_all.lock().unwrap();
        for s in sinks.iter() {
            let sink = s.lock().unwrap();
            // Si le volume est > 0, on coupe. Sinon on remet à 100%
            let current_vol = sink.volume();
            sink.set_volume(if current_vol > 0.0 { 0.0 } else { 1.0 });
        }
    });

    let window_clone = window.clone();
    let handle_clone = handle.clone();
    let sinks_for_add = Arc::clone(&all_sinks);
    
    add_track_btn.connect_clicked(move |_| {
        let dialog = FileChooserDialog::with_buttons(
            Some("Select Audio"), Some(&window_clone), FileChooserAction::Open,
            &[("_Cancel", ResponseType::Cancel), ("_Open", ResponseType::Accept)],
        );
        if dialog.run() == ResponseType::Accept {
            if let Some(filename) = dialog.get_filename() {
                create_track_row(&track_container, filename, handle_clone.as_ref(), &sinks_for_add);
            }
        }
        dialog.close();
    });

    window.connect_delete_event(|_, _| { gtk::main_quit(); Inhibit(false) });
    window
}

fn create_track_row(container: &Box, path: PathBuf, handle: Option<&Arc<OutputStreamHandle>>, all_sinks: &Arc<Mutex<Vec<Arc<Mutex<Sink>>>>>) {
    let track_box = Box::new(Orientation::Horizontal, 10);
    let play_btn = Button::with_label("▶");
    let mute_btn = Button::with_label("M"); // Bouton Mute individuel
    
    let cursor_x = Arc::new(Mutex::new(0.0));
    let current_progress = Arc::new(Mutex::new(0.0));
    let is_playing = Arc::new(Mutex::new(false));

    if let Some(h) = handle {
        if let Ok(sink) = Sink::try_new(h) {
            let sink_arc = Arc::new(Mutex::new(sink));
            all_sinks.lock().unwrap().push(Arc::clone(&sink_arc));

            let drawing_area = DrawingArea::new();
            drawing_area.set_size_request(600, 80);
            drawing_area.add_events(EventMask::POINTER_MOTION_MASK | EventMask::BUTTON_PRESS_MASK);

            // LOGIQUE MUTE INDIVIDUEL
            let s_mute = Arc::clone(&sink_arc);
            mute_btn.connect_clicked(move |btn| {
                let sink = s_mute.lock().unwrap();
                if sink.volume() > 0.0 {
                    sink.set_volume(0.0);
                    btn.set_label("UNMUTE");
                } else {
                    sink.set_volume(1.0);
                    btn.set_label("M");
                }
            });

            // NAVIGATION AU CLIC
            let p_click = Arc::clone(&current_progress);
            let da_click = drawing_area.clone();
            drawing_area.connect_button_press_event(move |_, event| {
                let (x, _) = event.get_position();
                *p_click.lock().unwrap() = x / 600.0; 
                da_click.queue_draw();
                Inhibit(false)
            });

            // SURVOL SOURIS
            let c_motion = Arc::clone(&cursor_x);
            let da_motion = drawing_area.clone();
            drawing_area.connect_motion_notify_event(move |_, event| {
                *c_motion.lock().unwrap() = event.get_position().0;
                da_motion.queue_draw();
                Inhibit(false)
            });

            // PROGRESSION RÉELLE
            let p_timer = Arc::clone(&current_progress);
            let playing_state = Arc::clone(&is_playing);
            let da_redraw = drawing_area.clone();
            glib::timeout_add_local(100, move || {
                if *playing_state.lock().unwrap() {
                    let mut p = p_timer.lock().unwrap();
                    if *p < 1.0 { 
                        *p += 0.001; 
                        da_redraw.queue_draw();
                    }
                }
                glib::Continue(true)
            });

            // DESSIN
            let p_draw = Arc::clone(&current_progress);
            let c_draw = Arc::clone(&cursor_x);
            drawing_area.connect_draw(move |_, cr| {
                let width = 600.0;
                let mid_y = 40.0;
                let p = *p_draw.lock().unwrap();
                let c = *c_draw.lock().unwrap();

                for i in (0..600).step_by(4) {
                    let x = i as f64;
                    if x / width < p { cr.set_source_rgb(0.0, 0.8, 0.2); } 
                    else { cr.set_source_rgb(0.2, 0.6, 1.0); } 

                    let h = ((i as f64 * 0.1).sin().abs() * 30.0) + 2.0;
                    cr.set_line_width(2.0);
                    cr.move_to(x, mid_y - h); cr.line_to(x, mid_y + h); cr.stroke();
                }

                cr.set_source_rgb(0.0, 1.0, 0.5); cr.set_line_width(2.0);
                cr.move_to(p * width, 0.0); cr.line_to(p * width, 80.0); cr.stroke();

                cr.set_source_rgb(1.0, 1.0, 1.0); cr.set_line_width(1.0);
                cr.move_to(c, 0.0); cr.line_to(c, 80.0); cr.stroke();
                Inhibit(false)
            });

            let file_path = path.to_str().unwrap().to_string();
            let s_play = Arc::clone(&sink_arc);
            let p_state = Arc::clone(&is_playing);
            play_btn.connect_clicked(move |btn| {
                let sink = s_play.lock().unwrap();
                let mut playing = p_state.lock().unwrap();
                if btn.get_label().unwrap() == "▶" {
                    if sink.empty() {
                        if let Ok(f) = File::open(&file_path) {
                            if let Ok(d) = Decoder::new(BufReader::new(f)) {
                                sink.append(d);
                            }
                        }
                    }
                    sink.play(); *playing = true; btn.set_label("⏸");
                } else {
                    sink.pause(); *playing = false; btn.set_label("▶");
                }
            });

            track_box.pack_start(&play_btn, false, false, 5);
            track_box.pack_start(&mute_btn, false, false, 5); // Ajout du bouton Mute dans le layout
            track_box.pack_start(&drawing_area, true, true, 5);
        }
    }
    
    let label = Label::new(Some(path.file_name().unwrap().to_str().unwrap()));
    track_box.pack_start(&label, false, false, 5);
    container.pack_start(&track_box, false, false, 5);
    container.show_all();
}
