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
use rodio::{buffer::SamplesBuffer, Decoder, OutputStream, Sink, OutputStreamHandle, Source};
use std::sync::{Arc, Mutex};
use std::f32::consts::PI;

// Traite un tampon complet : simple passe-bas (bass) + passe-haut (aigue)
fn apply_eq(
    samples: Vec<f32>,
    channels: u16,
    sample_rate: u32,
    bass_gain: f32,
    treble_gain: f32,
) -> Vec<f32> {
    let cutoff_hz = 200.0_f32;
    let dt = 1.0 / sample_rate as f32;
    let rc = 1.0 / (2.0 * PI * cutoff_hz);
    let alpha = dt / (rc + dt);

    let mut state = vec![0.0f32; channels as usize];
    let mut out = Vec::with_capacity(samples.len());

    for (i, s) in samples.into_iter().enumerate() {
        let ch = i % state.len();
        let prev = state[ch];
        let low = prev + alpha * (s - prev);
        state[ch] = low;

        let high = s - low;
        let mixed = s + bass_gain * low + treble_gain * high;
        out.push(mixed.clamp(-1.0, 1.0));
    }

    out
}

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
    let controls_column = Box::new(Orientation::Vertical, 5);

    // Ligne principale des controles
    let buttons_row = Box::new(Orientation::Horizontal, 5);
    let play_btn = Button::with_label("▶");
    let mute_btn = Button::with_label("M"); // Bouton Mute individuel
    let eq_btn = Button::with_label("Egaliseur"); // Bouton Egaliseur à droite du mute

    // Ligne d'EQ (masquee au depart)
    let bass_controls_row = Box::new(Orientation::Horizontal, 5);
    let bass_label = Label::new(Some("Bass"));
    let bass_minus_btn = Button::with_label("-");
    let bass_value_label = Label::new(Some("0"));
    let bass_plus_btn = Button::with_label("+");
    let bass_validate_btn = Button::with_label("Valider");
    bass_controls_row.pack_start(&bass_label, false, false, 0);
    bass_controls_row.pack_start(&bass_minus_btn, false, false, 0);
    bass_controls_row.pack_start(&bass_value_label, false, false, 0);
    bass_controls_row.pack_start(&bass_plus_btn, false, false, 0);
    bass_controls_row.pack_start(&bass_validate_btn, false, false, 0);
    bass_controls_row.hide();

    let treble_controls_row = Box::new(Orientation::Horizontal, 5);
    let treble_label = Label::new(Some("Aigue"));
    let treble_minus_btn = Button::with_label("-");
    let treble_value_label = Label::new(Some("0"));
    let treble_plus_btn = Button::with_label("+");
    let treble_validate_btn = Button::with_label("Valider");
    treble_controls_row.pack_start(&treble_label, false, false, 0);
    treble_controls_row.pack_start(&treble_minus_btn, false, false, 0);
    treble_controls_row.pack_start(&treble_value_label, false, false, 0);
    treble_controls_row.pack_start(&treble_plus_btn, false, false, 0);
    treble_controls_row.pack_start(&treble_validate_btn, false, false, 0);
    treble_controls_row.hide();
    
    let cursor_x = Arc::new(Mutex::new(0.0));
    let current_progress = Arc::new(Mutex::new(0.0));
    let is_playing = Arc::new(Mutex::new(false));
    let bass_level = Arc::new(Mutex::new(0_i32));
    let bass_gain = Arc::new(Mutex::new(0.0f32));
    let treble_level = Arc::new(Mutex::new(0_i32));
    let treble_gain = Arc::new(Mutex::new(0.0f32));

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

            // AFFICHAGE EQ
            let eq_row_toggle_bass = bass_controls_row.clone();
            let eq_row_toggle_treble = treble_controls_row.clone();
            eq_btn.connect_clicked(move |_| {
                let visible = eq_row_toggle_bass.is_visible();
                if visible {
                    eq_row_toggle_bass.hide();
                    eq_row_toggle_treble.hide();
                } else {
                    eq_row_toggle_bass.show_all();
                    eq_row_toggle_treble.show_all();
                }
            });

            // REGLAGE BASSES
            let bass_label_for_minus = bass_value_label.clone();
            let bass_label_for_plus = bass_value_label.clone();
            let bass_level_minus = Arc::clone(&bass_level);
            let bass_level_plus = Arc::clone(&bass_level);
            let bass_gain_minus = Arc::clone(&bass_gain);
            let bass_gain_plus = Arc::clone(&bass_gain);

            bass_minus_btn.connect_clicked(move |_| {
                let mut lvl = bass_level_minus.lock().unwrap();
                if *lvl > -10 { *lvl -= 1; }
                let gain = (*lvl as f32) * 0.1;
                *bass_gain_minus.lock().unwrap() = gain;
                bass_label_for_minus.set_text(&lvl.to_string());
            });

            bass_plus_btn.connect_clicked(move |_| {
                let mut lvl = bass_level_plus.lock().unwrap();
                if *lvl < 10 { *lvl += 1; }
                let gain = (*lvl as f32) * 0.1;
                *bass_gain_plus.lock().unwrap() = gain;
                bass_label_for_plus.set_text(&lvl.to_string());
            });

            // REGLAGE AIGUES
            let treble_label_for_minus = treble_value_label.clone();
            let treble_label_for_plus = treble_value_label.clone();
            let treble_level_minus = Arc::clone(&treble_level);
            let treble_level_plus = Arc::clone(&treble_level);
            let treble_gain_minus = Arc::clone(&treble_gain);
            let treble_gain_plus = Arc::clone(&treble_gain);

            treble_minus_btn.connect_clicked(move |_| {
                let mut lvl = treble_level_minus.lock().unwrap();
                if *lvl > -10 { *lvl -= 1; }
                let gain = (*lvl as f32) * 0.1;
                *treble_gain_minus.lock().unwrap() = gain;
                treble_label_for_minus.set_text(&lvl.to_string());
            });

            treble_plus_btn.connect_clicked(move |_| {
                let mut lvl = treble_level_plus.lock().unwrap();
                if *lvl < 10 { *lvl += 1; }
                let gain = (*lvl as f32) * 0.1;
                *treble_gain_plus.lock().unwrap() = gain;
                treble_label_for_plus.set_text(&lvl.to_string());
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
            let bass_gain_for_play = Arc::clone(&bass_gain);
            let treble_gain_for_play = Arc::clone(&treble_gain);

            // Données partagées pour les boutons "Valider"
            let path_for_validate_bass = file_path.clone();
            let path_for_validate_treble = file_path.clone();

            let s_validate_bass = Arc::clone(&sink_arc);
            let s_validate_treble = Arc::clone(&sink_arc);

            let bass_gain_for_validate_bass = Arc::clone(&bass_gain);
            let treble_gain_for_validate_bass = Arc::clone(&treble_gain);

            let bass_gain_for_validate_treble = Arc::clone(&bass_gain);
            let treble_gain_for_validate_treble = Arc::clone(&treble_gain);

            let progress_for_validate_bass = Arc::clone(&current_progress);
            let progress_for_validate_treble = Arc::clone(&current_progress);

            let playing_state_for_validate_bass = Arc::clone(&is_playing);
            let playing_state_for_validate_treble = Arc::clone(&is_playing);

            let play_btn_for_validate_bass = play_btn.clone();
            let play_btn_for_validate_treble = play_btn.clone();

            let da_for_validate_bass = drawing_area.clone();
            let da_for_validate_treble = drawing_area.clone();

            // VALIDER BASSES
            bass_validate_btn.connect_clicked(move |_| {
                if let Ok(f) = File::open(&path_for_validate_bass) {
                    if let Ok(decoder) = Decoder::new(BufReader::new(f)) {
                        let source = decoder.convert_samples::<f32>();
                        let channels = source.channels();
                        let rate = source.sample_rate();
                        let bass_gain_now = *bass_gain_for_validate_bass.lock().unwrap();
                        let treble_gain_now = *treble_gain_for_validate_bass.lock().unwrap();
                        let raw: Vec<f32> = source.collect();
                        let processed = apply_eq(raw, channels, rate, bass_gain_now, treble_gain_now);
                        let buffer = SamplesBuffer::new(channels, rate, processed);

                        let sink = s_validate_bass.lock().unwrap();
                        sink.pause();
                        sink.clear();
                        sink.append(buffer);
                        sink.play();

                        *progress_for_validate_bass.lock().unwrap() = 0.0;
                        *playing_state_for_validate_bass.lock().unwrap() = true;
                        play_btn_for_validate_bass.set_label("⏸");
                        da_for_validate_bass.queue_draw();
                    }
                }
            });

            // VALIDER AIGUES
            treble_validate_btn.connect_clicked(move |_| {
                if let Ok(f) = File::open(&path_for_validate_treble) {
                    if let Ok(decoder) = Decoder::new(BufReader::new(f)) {
                        let source = decoder.convert_samples::<f32>();
                        let channels = source.channels();
                        let rate = source.sample_rate();
                        let bass_gain_now = *bass_gain_for_validate_treble.lock().unwrap();
                        let treble_gain_now = *treble_gain_for_validate_treble.lock().unwrap();
                        let raw: Vec<f32> = source.collect();
                        let processed = apply_eq(raw, channels, rate, bass_gain_now, treble_gain_now);
                        let buffer = SamplesBuffer::new(channels, rate, processed);

                        let sink = s_validate_treble.lock().unwrap();
                        sink.pause();
                        sink.clear();
                        sink.append(buffer);
                        sink.play();

                        *progress_for_validate_treble.lock().unwrap() = 0.0;
                        *playing_state_for_validate_treble.lock().unwrap() = true;
                        play_btn_for_validate_treble.set_label("⏸");
                        da_for_validate_treble.queue_draw();
                    }
                }
            });

            play_btn.connect_clicked(move |btn| {
                let sink = s_play.lock().unwrap();
                let mut playing = p_state.lock().unwrap();
                if btn.get_label().unwrap() == "▶" {
                    if sink.empty() {
                        if let Ok(f) = File::open(&file_path) {
                            if let Ok(decoder) = Decoder::new(BufReader::new(f)) {
                                let source = decoder.convert_samples::<f32>();
                                let channels = source.channels();
                                let rate = source.sample_rate();
                                let bass_now = *bass_gain_for_play.lock().unwrap();
                                let treble_now = *treble_gain_for_play.lock().unwrap();
                                let raw: Vec<f32> = source.collect();
                                let processed = apply_eq(raw, channels, rate, bass_now, treble_now);
                                let buffer = SamplesBuffer::new(channels, rate, processed);
                                sink.append(buffer);
                            }
                        }
                    }
                    sink.play(); *playing = true; btn.set_label("⏸");
                } else {
                    sink.pause(); *playing = false; btn.set_label("▶");
                }
            });

            buttons_row.pack_start(&play_btn, false, false, 0);
            buttons_row.pack_start(&mute_btn, false, false, 0);
            buttons_row.pack_start(&eq_btn, false, false, 0);

            controls_column.pack_start(&buttons_row, false, false, 0);
            controls_column.pack_start(&bass_controls_row, false, false, 0);
            controls_column.pack_start(&treble_controls_row, false, false, 0);

            track_box.pack_start(&controls_column, false, false, 5);
            track_box.pack_start(&drawing_area, true, true, 5);
        }
    }
    
    let label = Label::new(Some(path.file_name().unwrap().to_str().unwrap()));
    track_box.pack_start(&label, false, false, 5);
    container.pack_start(&track_box, false, false, 5);
    container.show_all();
    bass_controls_row.hide(); // Reste cache tant qu'on n'a pas clique sur Egaliseur
    treble_controls_row.hide();
}
