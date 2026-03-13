use gtk::prelude::*;
use gtk::{
    Adjustment, FileChooserAction, FileChooserDialog, ResponseType,
    Window, WindowType, Box, Orientation, Button, Label, 
    ScrolledWindow, Toolbar, ToolButton, Settings, DrawingArea, Scale
};
use gdk::EventMask;
use glib; 
use std::path::PathBuf;
use std::fs::File;
use std::io::BufReader;
use rodio::{Decoder, OutputStream, Sink, OutputStreamHandle, Source};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, AtomicBool, Ordering};
use std::time::Duration;

// --- PROCESSEUR MULTI-EFFETS ---
struct DSPFilter<I> {
    input: I,
    bass_gain: Arc<AtomicU32>,
    prev_low: f32,
    disto_on: Arc<AtomicBool>,
    reverb_on: Arc<AtomicBool>,
    reverb_buffer: Vec<f32>,
    reverb_index: usize,
}

impl<I: Iterator<Item = f32>> Iterator for DSPFilter<I> {
    type Item = f32;
    fn next(&mut self) -> Option<Self::Item> {
        self.input.next().map(|x| {
            // Basses
            let alpha = 0.05; 
            let low = alpha * x + (1.0 - alpha) * self.prev_low;
            self.prev_low = low;
            let high = x - low;
            let bg = f32::from_bits(self.bass_gain.load(Ordering::Relaxed));
            let mut out = (low * bg) + high;

            // Distorsion (Saturation)
            if self.disto_on.load(Ordering::Relaxed) {
                let drive = 5.0; 
                out = (out * drive).tanh() * 0.7; 
            }

            // Reverb
            if self.reverb_on.load(Ordering::Relaxed) {
                let delay_sample = self.reverb_buffer[self.reverb_index];
                self.reverb_buffer[self.reverb_index] = out + delay_sample * 0.4;
                self.reverb_index = (self.reverb_index + 1) % self.reverb_buffer.len();
                out = (out * 0.7) + (delay_sample * 0.5);
            }

            out
        })
    }
}

impl<I: Source<Item = f32>> Source for DSPFilter<I> {
    fn current_frame_len(&self) -> Option<usize> { self.input.current_frame_len() }
    fn channels(&self) -> u16 { self.input.channels() }
    fn sample_rate(&self) -> u32 { self.input.sample_rate() }
    fn total_duration(&self) -> Option<Duration> { self.input.total_duration() }
}

#[allow(dead_code)]
pub fn run() {
    if gtk::init().is_err() { return; }
    let window = create_main_window();
    window.show_all();
    gtk::main();
}

pub fn create_main_window() -> Window {
    let settings = Settings::get_default().unwrap();
    settings.set_property_gtk_application_prefer_dark_theme(true);

    let window = Window::new(WindowType::Toplevel);
    window.set_title("MixRust - Professional DAW");
    window.set_default_size(1300, 700);

    let audio_data = OutputStream::try_default().ok();
    let (_stream, handle) = match audio_data {
        Some((s, h)) => (Some(s), Some(Arc::new(h))),
        None => (None, None),
    };

    if let Some(stream) = _stream {
        unsafe { window.set_data("mixrust_output_stream", stream); }
    }

    let main_vbox = Box::new(Orientation::Vertical, 0);
    window.add(&main_vbox);

    let toolbar = Toolbar::new();
    let add_track_btn = ToolButton::new::<Button>(None, Some("Add Track"));
    let play_all_btn = ToolButton::new::<Button>(None, Some("PLAY ALL"));
    let mute_all_btn = ToolButton::new::<Button>(None, Some("MUTE ALL")); 
    
    toolbar.insert(&add_track_btn, -1);
    toolbar.insert(&play_all_btn, -1);
    toolbar.insert(&mute_all_btn, -1);
    main_vbox.pack_start(&toolbar, false, false, 0);

    let all_sinks: Arc<Mutex<Vec<Arc<Mutex<Sink>>>>> = Arc::new(Mutex::new(Vec::new()));
    let scroll = ScrolledWindow::new(None::<&Adjustment>, None::<&Adjustment>);
    let track_container = Box::new(Orientation::Vertical, 5);
    scroll.add(&track_container);
    main_vbox.pack_start(&scroll, true, true, 0);

    let sinks_for_play = Arc::clone(&all_sinks);
    play_all_btn.connect_clicked(move |_| {
        for s in sinks_for_play.lock().unwrap().iter() { s.lock().unwrap().play(); }
    });

    let sinks_for_mute_all = Arc::clone(&all_sinks);
    mute_all_btn.connect_clicked(move |_| {
        let sinks = sinks_for_mute_all.lock().unwrap();
        for s in sinks.iter() {
            let sink = s.lock().unwrap();
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
    
    // BOUTONS PRINCIPAUX
    let restart_btn = Button::with_label("⏮");
    let play_btn = Button::with_label("▶");
    let mute_btn = Button::with_label("M"); 
    
    let vol_scale = Scale::with_range(Orientation::Horizontal, 0.0, 2.0, 0.1);
    vol_scale.set_value(1.0); vol_scale.set_size_request(70, -1);

    let speed_scale = Scale::with_range(Orientation::Horizontal, 0.25, 2.0, 0.05);
    speed_scale.set_value(1.0); speed_scale.set_size_request(70, -1);

    let bass_scale = Scale::with_range(Orientation::Horizontal, 0.0, 3.0, 0.1);
    bass_scale.set_value(1.0); bass_scale.set_size_request(70, -1);
    
    let disto_btn = Button::with_label("Distorsion");
    let reverb_btn = Button::with_label("Reverb");

    let bass_gain = Arc::new(AtomicU32::new(1f32.to_bits())); 
    let disto_on = Arc::new(AtomicBool::new(false));
    let reverb_on = Arc::new(AtomicBool::new(false));
    
    let cursor_x = Arc::new(Mutex::new(0.0));
    let current_progress = Arc::new(Mutex::new(0.0));
    let is_playing = Arc::new(Mutex::new(false));

    // --- ANALYSE PRÉCISE DE LA COURBE ET DURÉE EXACTE ---
    let mut amplitudes = Vec::new();
    let mut total_duration_secs = 1.0; 

    if let Ok(f) = File::open(&path) {
        if let Ok(d) = Decoder::new(BufReader::new(f)) {
            let channels = d.channels() as f64;
            let sample_rate = d.sample_rate() as f64;
            
            let samples: Vec<f32> = d.convert_samples::<f32>().collect();
            
            // Calcul ultra-précis du temps total en fonction des samples !
            if sample_rate > 0.0 && channels > 0.0 {
                total_duration_secs = samples.len() as f64 / (sample_rate * channels);
            }

            // Génération de la "Vraie Courbe" : On cherche le pic maximal (enveloppe) par zone.
            let num_points = 600;
            let chunk_size = (samples.len() / num_points).max(1);
            let mut max_overall_amp: f32 = 0.001; // Évite la division par 0
            
            for chunk in samples.chunks(chunk_size) {
                let mut local_max = 0.0f32;
                for &s in chunk {
                    let abs_s = s.abs();
                    if abs_s > local_max { local_max = abs_s; }
                }
                if local_max > max_overall_amp { max_overall_amp = local_max; }
                amplitudes.push(local_max);
                if amplitudes.len() == num_points { break; } // Sécurité
            }

            // Normalisation : On grossit la courbe pour qu'elle remplisse tout l'espace proprement
            for a in amplitudes.iter_mut() {
                *a /= max_overall_amp; 
            }
        }
    }
    if amplitudes.is_empty() { amplitudes.resize(600, 0.0); } 
    let amps_arc = Arc::new(amplitudes);

    if let Some(h) = handle {
        if let Ok(sink) = Sink::try_new(h) {
            let sink_arc = Arc::new(Mutex::new(sink));
            all_sinks.lock().unwrap().push(Arc::clone(&sink_arc));

            let drawing_area = DrawingArea::new();
            drawing_area.set_size_request(500, 80); 
            drawing_area.add_events(EventMask::POINTER_MOTION_MASK);

            // LOGIQUE VOLUME ET VITESSE
            let s_vol = Arc::clone(&sink_arc);
            vol_scale.connect_value_changed(move |sc| { s_vol.lock().unwrap().set_volume(sc.get_value() as f32); });

            let s_speed = Arc::clone(&sink_arc);
            speed_scale.connect_value_changed(move |sc| { s_speed.lock().unwrap().set_speed(sc.get_value() as f32); });

            // LOGIQUE BASSES ET EFFETS
            let bg_clone = Arc::clone(&bass_gain);
            bass_scale.connect_value_changed(move |sc| { bg_clone.store((sc.get_value() as f32).to_bits(), Ordering::Relaxed); });

            let d_state = Arc::clone(&disto_on);
            disto_btn.connect_clicked(move |btn| {
                let current = d_state.load(Ordering::Relaxed);
                d_state.store(!current, Ordering::Relaxed);
                if !current { btn.set_label("Distorsion: ON"); } else { btn.set_label("Distorsion"); }
            });

            let r_state = Arc::clone(&reverb_on);
            reverb_btn.connect_clicked(move |btn| {
                let current = r_state.load(Ordering::Relaxed);
                r_state.store(!current, Ordering::Relaxed);
                if !current { btn.set_label("Reverb: ON"); } else { btn.set_label("Reverb"); }
            });

            // LOGIQUE MUTE
            let s_mute = Arc::clone(&sink_arc);
            let v_scale_mute = vol_scale.clone();
            mute_btn.connect_clicked(move |btn| {
                let sink = s_mute.lock().unwrap();
                if sink.volume() > 0.0 {
                    sink.set_volume(0.0); btn.set_label("UNMUTE");
                } else {
                    sink.set_volume(v_scale_mute.get_value() as f32); btn.set_label("M");
                }
            });

            // SURVOL SOURIS
            let c_motion = Arc::clone(&cursor_x);
            let da_motion = drawing_area.clone();
            drawing_area.connect_motion_notify_event(move |_, event| {
                *c_motion.lock().unwrap() = event.get_position().0;
                da_motion.queue_draw();
                Inhibit(false)
            });

            // --- NOUVEAU: PROGRESSION INTELLIGENTE SYNCHRONISÉE ---
            let p_timer = Arc::clone(&current_progress);
            let playing_state = Arc::clone(&is_playing);
            let da_redraw = drawing_area.clone();
            let sc_speed_timer = speed_scale.clone();

            glib::timeout_add_local(100, move || {
                if *playing_state.lock().unwrap() {
                    let mut p = p_timer.lock().unwrap();
                    if *p < 1.0 {
                        // 100ms = 0.1 secondes réelles écoulées
                        let elapsed_time = 0.1 * sc_speed_timer.get_value() as f64;
                        // Avancement par rapport au total de la musique
                        *p += elapsed_time / total_duration_secs;
                        
                        if *p >= 1.0 { *p = 1.0; } // Bloque à la fin proprement
                        da_redraw.queue_draw();
                    }
                }
                glib::Continue(true)
            });

            // DESSIN AVEC VRAIES COURBES NORMALISÉES
            let p_draw = Arc::clone(&current_progress);
            let c_draw = Arc::clone(&cursor_x);
            let amps_draw = Arc::clone(&amps_arc);
            drawing_area.connect_draw(move |da, cr| {
                let width = da.get_allocated_width() as f64;
                let mid_y = 40.0;
                let p = *p_draw.lock().unwrap();
                let c = *c_draw.lock().unwrap();

                // On dessine l'enveloppe sonore extraite !
                for i in (0..600).step_by(4) {
                    let x = i as f64 * (width / 600.0);
                    if x / width < p { cr.set_source_rgb(0.0, 0.8, 0.2); } 
                    else { cr.set_source_rgb(0.2, 0.6, 1.0); } 

                    let amp = if i < amps_draw.len() { amps_draw[i] as f64 } else { 0.0 };
                    let h = (amp * 38.0).max(1.0); // Hauteur visuelle basée sur le vrai volume
                    
                    cr.set_line_width(2.0);
                    cr.move_to(x, mid_y - h); cr.line_to(x, mid_y + h); cr.stroke();
                }

                cr.set_source_rgb(1.0, 0.2, 0.2); cr.set_line_width(2.0);
                cr.move_to(p * width, 0.0); cr.line_to(p * width, 80.0); cr.stroke();
                cr.set_source_rgb(1.0, 1.0, 1.0); cr.set_line_width(1.0);
                cr.move_to(c, 0.0); cr.line_to(c, 80.0); cr.stroke();
                Inhibit(false)
            });

            // LOGIQUE LECTURE (PLAY)
            let file_path = path.to_str().unwrap().to_string();
            let s_play = Arc::clone(&sink_arc);
            let p_state = Arc::clone(&is_playing);
            let bg_play = Arc::clone(&bass_gain);
            let dist_play = Arc::clone(&disto_on);
            let rev_play = Arc::clone(&reverb_on);
            
            play_btn.connect_clicked(move |btn| {
                let sink = s_play.lock().unwrap();
                let mut playing = p_state.lock().unwrap();
                if btn.get_label().unwrap() == "▶" {
                    if sink.empty() {
                        if let Ok(f) = File::open(&file_path) {
                            if let Ok(d) = Decoder::new(BufReader::new(f)) {
                                let source = d.convert_samples::<f32>();
                                let filtered = DSPFilter {
                                    input: source,
                                    bass_gain: Arc::clone(&bg_play),
                                    prev_low: 0.0,
                                    disto_on: Arc::clone(&dist_play),
                                    reverb_on: Arc::clone(&rev_play),
                                    reverb_buffer: vec![0.0; 8000], 
                                    reverb_index: 0,
                                };
                                sink.append(filtered);
                            }
                        }
                    }
                    sink.play(); *playing = true; btn.set_label("⏸");
                } else {
                    sink.pause(); *playing = false; btn.set_label("▶");
                }
            });

            // LOGIQUE RECOMMENCER
            let s_restart = Arc::clone(&sink_arc);
            let p_restart = Arc::clone(&current_progress);
            let play_btn_restart = play_btn.clone();
            let path_restart = path.clone();
            let bg_restart = Arc::clone(&bass_gain);
            let dist_restart = Arc::clone(&disto_on);
            let rev_restart = Arc::clone(&reverb_on);
            let is_playing_restart = Arc::clone(&is_playing);

            restart_btn.connect_clicked(move |_| {
                let sink = s_restart.lock().unwrap();
                sink.stop(); 
                *p_restart.lock().unwrap() = 0.0; 
                
                if let Ok(f) = File::open(&path_restart) {
                    if let Ok(d) = Decoder::new(BufReader::new(f)) {
                        let source = d.convert_samples::<f32>();
                        let filtered = DSPFilter {
                            input: source,
                            bass_gain: Arc::clone(&bg_restart),
                            prev_low: 0.0,
                            disto_on: Arc::clone(&dist_restart),
                            reverb_on: Arc::clone(&rev_restart),
                            reverb_buffer: vec![0.0; 8000], 
                            reverb_index: 0,
                        };
                        sink.append(filtered);
                    }
                }
                sink.play();
                *is_playing_restart.lock().unwrap() = true;
                play_btn_restart.set_label("⏸");
            });

            track_box.pack_start(&restart_btn, false, false, 2);
            track_box.pack_start(&play_btn, false, false, 2);
            track_box.pack_start(&mute_btn, false, false, 2);
            
            track_box.pack_start(&Label::new(Some("Vol")), false, false, 0);
            track_box.pack_start(&vol_scale, false, false, 2);
            
            track_box.pack_start(&Label::new(Some("Vit.")), false, false, 0);
            track_box.pack_start(&speed_scale, false, false, 2);
            
            track_box.pack_start(&Label::new(Some("Bass")), false, false, 0);
            track_box.pack_start(&bass_scale, false, false, 2);

            track_box.pack_start(&disto_btn, false, false, 2);
            track_box.pack_start(&reverb_btn, false, false, 2);

            track_box.pack_start(&drawing_area, true, true, 5);
        }
    }
    
    let label = Label::new(Some(path.file_name().unwrap().to_str().unwrap()));
    track_box.pack_start(&label, false, false, 5);
    container.pack_start(&track_box, false, false, 5);
    container.show_all();
}