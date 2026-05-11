use gtk::prelude::*;
use gtk::{
    Adjustment, FileChooserAction, FileChooserDialog, ResponseType,
    Window, WindowType, Box, Orientation, Button, Label,
    ScrolledWindow, Toolbar, ToolButton, Settings, DrawingArea, Scale,
    CssProvider, StyleContext, EventBox
};
use gdk::EventMask;
use glib; 
use std::path::PathBuf;
use std::fs::File;
use std::io::BufReader;
// C'est ICI qu'il manquait 'Source' :
use rodio::{Decoder, OutputStream, Sink, OutputStreamHandle, Source, buffer::SamplesBuffer};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use hound;
use std::fs;
use std::time::Instant;
use gdk::Screen;
use std::collections::HashMap;

#[derive(Clone, Debug)]
struct RecordedEvent {
    track_id: u32,
    start_offset_secs: f64,
    volume: f32,
}

#[derive(Debug)]
struct RecordingState {
    active: bool,
    start_instant: Option<Instant>,
    session_duration_secs: f64,
    events: Vec<RecordedEvent>,
}

impl RecordingState {
    fn new() -> Self {
        Self { active: false, start_instant: None, session_duration_secs: 0.0, events: Vec::new() }
    }
}

// --- ETAT GLOBAL DE LA PISTE ---
struct TrackState {
    id: u32,
    name: String,
    path: PathBuf,
    sink: Arc<Mutex<Sink>>,
    is_playing: Arc<Mutex<bool>>,
    progress: Arc<Mutex<f64>>,
    volume: Rc<Cell<f64>>,
    speed: Rc<Cell<f64>>,
    bass_gain: Arc<AtomicU32>, 
    
    audio_buffer: Arc<Mutex<Vec<f32>>>,
    channels: u16,
    sample_rate: u32,
    
    total_samples: Rc<Cell<usize>>,
    total_duration_secs: Rc<Cell<f64>>,
    
    amplitudes: Arc<Mutex<Vec<f32>>>,
    colors: Arc<Mutex<Vec<(f64, f64, f64)>>>, 
    effect_window: Arc<Mutex<(f64, f64)>>,
}

// --- WIDGETS DU RUBAN ---
#[derive(Clone)]
struct RibbonUI {
    label_active: Label,
    scale_vol: Scale,
    scale_speed: Scale,
    scale_bass: Scale,
    btn_apply_vol: Button,
    btn_apply_speed: Button,
    btn_apply_bass: Button,
    btn_apply_disto: Button,
    btn_apply_reverb: Button,
}

// --- STYLES CSS ---
fn load_custom_css() {
    let provider = CssProvider::new();
    let css_data = b"
        #rec-start-recording.recording { background: #c62828; color: white; border-radius: 6px; font-weight: bold; }
        .track-row { background-color: #2b2b2b; border: 2px solid #3c3c3c; border-radius: 8px; padding: 10px; }
        .track-row-selected { border: 2px solid #4fc3f7; }
        .track-label { font-size: 14px; font-weight: bold; color: #4fc3f7; }
        .control-label { font-size: 11px; color: #aaaaaa; font-weight: bold; }
        .remove-btn { background: transparent; color: #ef5350; border: 1px solid #ef5350; border-radius: 4px; padding: 0px; font-size: 10px;}
        .remove-btn:hover { background: #ef5350; color: white; }
        .ribbon-panel { background-color: #1e1e1e; border-bottom: 2px solid #3c3c3c; padding: 10px; }
        .mini-btn { font-size: 12px; padding: 2px 5px; }
        
        .btn-disto { background-color: #ff8c00; color: white; font-weight: bold; }
        .btn-reverb { background-color: #9c27b0; color: white; font-weight: bold; }
        .btn-bass { background-color: #00acc1; color: white; font-weight: bold; }
        .btn-vol { background-color: #4caf50; color: white; font-weight: bold; }
        .btn-speed { background-color: #f44336; color: white; font-weight: bold; }
    ";
    provider.load_from_data(css_data).expect("CSS Error");
    if let Some(screen) = Screen::get_default() {
        StyleContext::add_provider_for_screen(&screen, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

// --- FONCTIONS ENREGISTREMENT ---
fn start_recording_session(recording_state: &Arc<Mutex<RecordingState>>) {
    let mut state = recording_state.lock().unwrap();
    state.active = true;
    state.start_instant = Some(Instant::now());
    state.events.clear();
    println!("Enregistrement démarré");
}

fn stop_recording_session(recording_state: &Arc<Mutex<RecordingState>>) {
    let mut state = recording_state.lock().unwrap();
    if let Some(start) = state.start_instant {
        state.session_duration_secs = start.elapsed().as_secs_f64();
    } else {
        state.session_duration_secs = 0.0;
    }
    state.active = false;
    state.start_instant = None;
    println!("Enregistrement arrêté (durée = {:.2}s)", state.session_duration_secs);
}

fn get_session_duration(recording_state: &Arc<Mutex<RecordingState>>) -> f64 {
    recording_state.lock().unwrap().session_duration_secs
}

fn get_recorded_events(recording_state: &Arc<Mutex<RecordingState>>) -> Vec<RecordedEvent> {
    recording_state.lock().unwrap().events.clone()
}

// --- RECALCUL DU GRAPHISME ---
fn recompute_amps(samples: &[f32], amps: &mut [f32]) {
    if samples.is_empty() { return; }
    let chunk_size = (samples.len() / 600).max(1);
    let mut max_amp = 0.001;
    let mut temp = vec![0.0; 600];
    for (i, chunk) in samples.chunks(chunk_size).take(600).enumerate() {
        let local_max = chunk.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        if local_max > max_amp { max_amp = local_max; }
        temp[i] = local_max;
    }
    for i in 0..600 { amps[i] = temp[i] / max_amp; }
}

fn color_chunks(colors: &mut [(f64, f64, f64)], start_ratio: f64, end_ratio: f64, color: (f64, f64, f64)) {
    let start_idx = (start_ratio * 600.0) as usize;
    let end_idx = (end_ratio * 600.0) as usize;
    for i in start_idx..=end_idx.min(599) {
        colors[i] = color;
    }
}

// --- JOUER UN MORCEAU (SEEKING) ---
fn restart_playback_seamless(track: &TrackState, force_play: bool) {
    let mut is_playing = track.is_playing.lock().unwrap();
    let ratio = *track.progress.lock().unwrap();
    let sink = track.sink.lock().unwrap();
    
    sink.stop();
    
    if *is_playing || force_play {
        let buf = track.audio_buffer.lock().unwrap();
        let start_idx = (ratio * buf.len() as f64) as usize;
        
        let c = track.channels as usize;
        let aligned_idx = (start_idx / c) * c;
        
        if aligned_idx < buf.len() {
            let slice = buf[aligned_idx..].to_vec();
            let source = SamplesBuffer::new(track.channels, track.sample_rate, slice);
            sink.append(source);
            sink.play();
            *is_playing = true;
        }
    }
}

// --- APPLICATION DES EFFETS EN MEMOIRE ---
fn apply_effect_to_track(track: &TrackState, effect: &str) {
    let window = *track.effect_window.lock().unwrap();
    let mut buf = track.audio_buffer.lock().unwrap();
    let mut amps = track.amplitudes.lock().unwrap();
    let mut colors = track.colors.lock().unwrap();
    
    let total = buf.len();
    let c = track.channels as usize;
    if c == 0 { return; }

    let mut start = (window.0 * total as f64) as usize;
    let mut end = (window.1 * total as f64) as usize;
    
    start = (start / c) * c;
    end = (end / c) * c;
    if start >= end { return; }

    if effect == "disto" {
        for i in start..end { buf[i] = (buf[i] * 5.0).tanh() * 0.7; }
        color_chunks(&mut colors, window.0, window.1, (1.0, 0.55, 0.0));
    } 
    else if effect == "reverb" {
        let mut delay_buf = vec![0.0; 8000];
        let mut idx = 0;
        for i in start..end {
            let s = buf[i];
            let d = delay_buf[idx];
            delay_buf[idx] = s + d * 0.4;
            idx = (idx + 1) % 8000;
            buf[i] = (s * 0.7) + (d * 0.5);
        }
        color_chunks(&mut colors, window.0, window.1, (0.61, 0.15, 0.69));
    }
    else if effect == "bass" {
        let gain = f32::from_bits(track.bass_gain.load(Ordering::Relaxed));
        let mut prev = 0.0;
        for i in start..end {
            let s = buf[i];
            let low = 0.05 * s + 0.95 * prev;
            prev = low;
            let high = s - low;
            buf[i] = (low * gain) + high;
        }
        color_chunks(&mut colors, window.0, window.1, (0.0, 0.67, 0.75));
    }
    else if effect == "volume" {
        let vol = track.volume.get() as f32;
        for i in start..end {
            buf[i] = (buf[i] * vol).max(-1.0).min(1.0);
        }
        color_chunks(&mut colors, window.0, window.1, (0.3, 0.69, 0.3));
    }
    else if effect == "speed" {
        let speed = track.speed.get();
        if (speed - 1.0).abs() > 0.01 {
            let old_frames = (end - start) / c;
            let new_frames = (old_frames as f64 / speed) as usize;
            let mut new_window = Vec::with_capacity(new_frames * c);
            
            for i in 0..new_frames {
                let orig_frame = (i as f64 * speed) as usize;
                let orig_idx = start + orig_frame * c;
                for ch in 0..c {
                    if orig_idx + ch < buf.len() {
                        new_window.push(buf[orig_idx + ch]);
                    } else {
                        new_window.push(0.0);
                    }
                }
            }
            buf.splice(start..end, new_window);
            
            let new_total = buf.len();
            track.total_samples.set(new_total);
            track.total_duration_secs.set(new_total as f64 / (track.sample_rate as f64 * c as f64));
            
            color_chunks(&mut colors, window.0, window.1, (0.95, 0.26, 0.21));
        }
    }

    recompute_amps(&buf, &mut amps);
    drop(buf);
    restart_playback_seamless(track, false);
}

// --- EXPORT GLOBAL DU MIX ---
fn export_recorded_session(registry: &HashMap<u32, TrackState>, events: &[RecordedEvent], session_duration_secs: f64, output_path: &PathBuf) -> Result<(), std::boxed::Box<dyn std::error::Error>> {
    if events.is_empty() { return Err("Aucun événement enregistré".into()); }
    
    let mut ref_channels = 2;
    let mut ref_sample_rate = 44100;
    let mut total_len = 0;
    
    for event in events {
        if let Some(track) = registry.get(&event.track_id) {
            ref_channels = track.channels;
            ref_sample_rate = track.sample_rate;
            let remaining_secs = session_duration_secs - event.start_offset_secs;
            if remaining_secs > 0.0 {
                let end_len = (event.start_offset_secs * ref_sample_rate as f64 * ref_channels as f64) as usize + track.audio_buffer.lock().unwrap().len();
                if end_len > total_len { total_len = end_len; }
            }
        }
    }

    let c_usize = ref_channels as usize;
    total_len = (total_len / c_usize) * c_usize;
    let mut mix_buffer = vec![0.0f32; total_len];

    for event in events {
        if let Some(track) = registry.get(&event.track_id) {
            let buf = track.audio_buffer.lock().unwrap();
            let raw_offset = (event.start_offset_secs * ref_sample_rate as f64 * ref_channels as f64) as usize;
            let offset = (raw_offset / c_usize) * c_usize;
            let vol = event.volume;

            for (i, sample) in buf.iter().enumerate() {
                let pos = offset + i;
                if pos < mix_buffer.len() { mix_buffer[pos] += *sample * vol; }
            }
        }
    }

    for s in mix_buffer.iter_mut() { *s = s.max(-1.0).min(1.0); }

    let spec = hound::WavSpec { channels: ref_channels, sample_rate: ref_sample_rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut writer = hound::WavWriter::create(output_path, spec)?;
    for &sample in mix_buffer.iter() {
        writer.write_sample((sample * i16::MAX as f32) as i16)?;
    }
    writer.finalize()?;
    Ok(())
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
    load_custom_css();

    let window = Window::new(WindowType::Toplevel);
    window.set_title("MixRust - Professional DAW");
    window.set_default_size(1300, 750);

    let audio_data = OutputStream::try_default().ok();
    let (_stream, handle) = match audio_data {
        Some((s, h)) => (Some(s), Some(Arc::new(h))),
        None => (None, None),
    };

    if let Some(stream) = _stream {
        unsafe { window.set_data("mixrust_output_stream", stream); }
    }

    let recording_state = Arc::new(Mutex::new(RecordingState::new()));
    let track_registry: Rc<RefCell<HashMap<u32, TrackState>>> = Rc::new(RefCell::new(HashMap::new()));
    let active_track_id: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
    let next_track_id: Rc<Cell<u32>> = Rc::new(Cell::new(1));
    let all_sinks: Arc<Mutex<Vec<Arc<Mutex<Sink>>>>> = Arc::new(Mutex::new(Vec::new()));

    let main_vbox = Box::new(Orientation::Vertical, 0);
    window.add(&main_vbox);

    let toolbar = Toolbar::new();
    let add_track_btn = ToolButton::new::<Button>(None, Some("Add Track"));
    let play_all_btn = ToolButton::new::<Button>(None, Some("PLAY ALL"));
    let mute_all_btn = ToolButton::new::<Button>(None, Some("MUTE ALL"));
    let rec_start_btn = ToolButton::new::<Button>(None, Some("REC START"));
    rec_start_btn.set_widget_name("rec-start-recording");
    let rec_stop_btn = ToolButton::new::<Button>(None, Some("REC STOP"));

    toolbar.insert(&add_track_btn, -1);
    toolbar.insert(&play_all_btn, -1);
    toolbar.insert(&mute_all_btn, -1);
    toolbar.insert(&rec_start_btn, -1);
    toolbar.insert(&rec_stop_btn, -1);
    main_vbox.pack_start(&toolbar, false, false, 0);

    let all_sinks: Arc<Mutex<Vec<Arc<Mutex<Sink>>>>> = Arc::new(Mutex::new(Vec::new()));
    let scroll = ScrolledWindow::new(None::<&Adjustment>, None::<&Adjustment>);
    let track_container = Box::new(Orientation::Vertical, 5);
    scroll.add(&track_container);
    main_vbox.pack_start(&scroll, true, true, 0);

    let ribbon_ui = RibbonUI {
        label_active: ribbon_header,
        scale_vol: r_scale_vol,
        scale_speed: r_scale_speed,
        scale_bass: r_scale_bass,
        btn_apply_vol: r_btn_apply_vol,
        btn_apply_speed: r_btn_apply_speed,
        btn_apply_bass: r_btn_apply_bass,
        btn_apply_disto: r_btn_apply_disto,
        btn_apply_reverb: r_btn_apply_reverb,
    };

    // --- LOGIQUE DU RUBAN (SLIDERS) ---
    let reg_vol = track_registry.clone();
    let act_vol = active_track_id.clone();
    ribbon_ui.scale_vol.connect_value_changed(move |sc| {
        if let Some(id) = act_vol.get() {
            if let Some(track) = reg_vol.borrow().get(&id) {
                let val = sc.get_value();
                track.volume.set(val);
                track.sink.lock().unwrap().set_volume(val as f32);
            }
        }
    });

    let reg_speed = track_registry.clone();
    let act_speed = active_track_id.clone();
    ribbon_ui.scale_speed.connect_value_changed(move |sc| {
        if let Some(id) = act_speed.get() {
            if let Some(track) = reg_speed.borrow().get(&id) {
                let val = sc.get_value();
                track.speed.set(val);
            }
        }
    });

    let reg_bass_scale = track_registry.clone();
    let act_bass_scale = active_track_id.clone();
    ribbon_ui.scale_bass.connect_value_changed(move |sc| {
        if let Some(id) = act_bass_scale.get() {
            if let Some(track) = reg_bass_scale.borrow().get(&id) {
                track.bass_gain.store((sc.get_value() as f32).to_bits(), Ordering::Relaxed);
            }
        }
    });

    // --- APPLICATION DES EFFETS (BOUTONS) ---
    let reg_vol_apply = track_registry.clone();
    let act_vol_apply = active_track_id.clone();
    let ribbon_vol_ui = ribbon_ui.clone();
    ribbon_ui.btn_apply_vol.connect_clicked(move |_| {
        if let Some(id) = act_vol_apply.get() {
            if let Some(track) = reg_vol_apply.borrow().get(&id) { 
                apply_effect_to_track(track, "volume"); 
                ribbon_vol_ui.scale_vol.set_value(1.0);
            }
        }
    });

    let reg_speed_apply = track_registry.clone();
    let act_speed_apply = active_track_id.clone();
    let ribbon_speed_ui = ribbon_ui.clone();
    ribbon_ui.btn_apply_speed.connect_clicked(move |_| {
        if let Some(id) = act_speed_apply.get() {
            if let Some(track) = reg_speed_apply.borrow().get(&id) { 
                apply_effect_to_track(track, "speed"); 
                ribbon_speed_ui.scale_speed.set_value(1.0);
            }
        }
    });

    let reg_bass = track_registry.clone();
    let act_bass = active_track_id.clone();
    let ribbon_bass_ui = ribbon_ui.clone();
    ribbon_ui.btn_apply_bass.connect_clicked(move |_| {
        if let Some(id) = act_bass.get() {
            if let Some(track) = reg_bass.borrow().get(&id) { 
                apply_effect_to_track(track, "bass"); 
                ribbon_bass_ui.scale_bass.set_value(1.0);
            }
        }
    });

    let reg_dist = track_registry.clone();
    let act_dist = active_track_id.clone();
    ribbon_ui.btn_apply_disto.connect_clicked(move |_| {
        if let Some(id) = act_dist.get() {
            if let Some(track) = reg_dist.borrow().get(&id) { apply_effect_to_track(track, "disto"); }
        }
    });

    let reg_rev = track_registry.clone();
    let act_rev = active_track_id.clone();
    ribbon_ui.btn_apply_reverb.connect_clicked(move |_| {
        if let Some(id) = act_rev.get() {
            if let Some(track) = reg_rev.borrow().get(&id) { apply_effect_to_track(track, "reverb"); }
        }
    });

    // --- BOUTONS GLOBAUX ---
    let sinks_for_play = Arc::clone(&all_sinks);
    play_all_btn.connect_clicked(move |_| { for s in sinks_for_play.lock().unwrap().iter() { s.lock().unwrap().play(); }});
    
    let sinks_for_mute = Arc::clone(&all_sinks);
    mute_all_btn.connect_clicked(move |_| { 
        for s in sinks_for_mute.lock().unwrap().iter() { 
            let sink = s.lock().unwrap(); 
            sink.set_volume(if sink.volume() > 0.0 { 0.0 } else { 1.0 }); 
        }
    });

    let rec_state_start = Arc::clone(&recording_state);
    let rec_start_btn_clone = rec_start_btn.clone();
    rec_start_btn.connect_clicked(move |_| {
        start_recording_session(&rec_state_start);
        rec_start_btn_clone.get_style_context().add_class("recording");
        rec_start_btn_clone.set_label(Some("REC ●"));
    });

    let rec_state_stop = Arc::clone(&recording_state);
    let rec_start_btn_stop = rec_start_btn.clone();
    let reg_export = track_registry.clone();
    rec_stop_btn.connect_clicked(move |_| {
        stop_recording_session(&rec_state_stop);
        rec_start_btn_stop.get_style_context().remove_class("recording");
        rec_start_btn_stop.set_label(Some("REC START"));
        
        let folder = PathBuf::from("enregistrements");
        if !folder.exists() { let _ = fs::create_dir_all(&folder); }
        let output_path = folder.join("mix_session.wav");
        
        let dur = get_session_duration(&rec_state_stop);
        let evts = get_recorded_events(&rec_state_stop);
        
        match export_recorded_session(&reg_export.borrow(), &evts, dur, &output_path) {
            Ok(_) => println!("Mix exporté : {}", output_path.display()),
            Err(e) => eprintln!("Erreur export session : {}", e),
        }
    });

    let window_clone = window.clone();
    let handle_clone = handle.clone();
    let ribbon_ui_clone = ribbon_ui.clone();
    
    add_track_btn.connect_clicked(move |_| {
        let dialog = FileChooserDialog::with_buttons(
            Some("Select Audio"), Some(&window_clone), FileChooserAction::Open,
            &[("_Cancel", ResponseType::Cancel), ("_Open", ResponseType::Accept)],
        );
        if dialog.run() == ResponseType::Accept {
            if let Some(filename) = dialog.get_filename() {
                create_track_row(
                    &track_container, filename, handle_clone.as_ref(), &all_sinks, &recording_state,
                    &track_registry, &active_track_id, &next_track_id, &ribbon_ui_clone
                );
            }
        }
        dialog.close();
    });

    window.connect_delete_event(|_, _| { gtk::main_quit(); Inhibit(false) });
    window
}

fn update_ribbon_from_state(ribbon: &RibbonUI, track: &TrackState) {
    ribbon.label_active.set_text(&format!("Cible actuelle : {}", track.name));
    ribbon.scale_vol.set_value(track.volume.get());
    ribbon.scale_speed.set_value(track.speed.get());
    ribbon.scale_bass.set_value(f32::from_bits(track.bass_gain.load(Ordering::Relaxed)) as f64);
}

fn create_track_row(
    container: &Box,
    path: PathBuf,
    handle: Option<&Arc<OutputStreamHandle>>,
    all_sinks: &Arc<Mutex<Vec<Arc<Mutex<Sink>>>>>,
    recording_state: &Arc<Mutex<RecordingState>>,
    track_registry: &Rc<RefCell<HashMap<u32, TrackState>>>,
    active_track_id: &Rc<Cell<Option<u32>>>,
    next_track_id: &Rc<Cell<u32>>,
    ribbon_ui: &RibbonUI,
) {
    let track_id = next_track_id.get();
    next_track_id.set(track_id + 1);
    
    let full_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("Piste").to_string();
    let display_name = if full_name.chars().count() > 10 {
        format!("{}...", full_name.chars().take(10).collect::<String>())
    } else { full_name.clone() };

    let track_box = Box::new(Orientation::Horizontal, 15);
    track_box.get_style_context().add_class("track-row");
    
    let event_box = EventBox::new();
    event_box.add(&track_box);

    let is_alive = Rc::new(Cell::new(true));
    let mut sink_for_remove: Option<Arc<Mutex<Sink>>> = None;

    let remove_btn = Button::with_label("✖");
    remove_btn.set_tooltip_text(Some("Supprimer"));
    remove_btn.get_style_context().add_class("remove-btn");
    remove_btn.set_size_request(24, 24);

    let restart_btn = Button::with_label("⏮");
    restart_btn.get_style_context().add_class("mini-btn");
    let play_btn = Button::with_label("▶");
    play_btn.get_style_context().add_class("mini-btn");
    let mute_btn = Button::with_label("M");
    mute_btn.get_style_context().add_class("mini-btn");
    let export_btn = Button::with_label("💾");
    export_btn.get_style_context().add_class("mini-btn");
    export_btn.set_tooltip_text(Some("Exporter uniquement cette piste"));

    let bass_gain = Arc::new(AtomicU32::new(1f32.to_bits()));
    let vol_val = Rc::new(Cell::new(1.0));
    let speed_val = Rc::new(Cell::new(1.0));
    let effect_window = Arc::new(Mutex::new((0.0, 1.0)));
    
    let cursor_x = Arc::new(Mutex::new(0.0));
    let current_progress = Arc::new(Mutex::new(0.0));
    let is_playing = Arc::new(Mutex::new(false));

    let is_dragging = Arc::new(Mutex::new(false));
    let drag_start = Arc::new(Mutex::new(0.0));

    let mut amplitudes = vec![0.0; 600];
    let colors = vec![(0.31, 0.76, 0.96); 600]; // Bleu clair par défaut
    
    let mut total_duration_secs = 1.0;
    let mut audio_vec = Vec::new();
    let mut s_rate = 44100;
    let mut c_count = 2;

    if let Ok(f) = File::open(&path) {
        if let Ok(d) = Decoder::new(BufReader::new(f)) {
            c_count = d.channels();
            s_rate = d.sample_rate();
            audio_vec = d.convert_samples::<f32>().collect();

            if s_rate > 0 && c_count > 0 { total_duration_secs = audio_vec.len() as f64 / (s_rate as f64 * c_count as f64); }
            recompute_amps(&audio_vec, &mut amplitudes);
        }
    }

    let audio_arc = Arc::new(Mutex::new(audio_vec));
    let amps_arc = Arc::new(Mutex::new(amplitudes));
    let cols_arc = Arc::new(Mutex::new(colors));
    
    let dur_cell = Rc::new(Cell::new(total_duration_secs));
    let samples_cell = Rc::new(Cell::new(audio_arc.lock().unwrap().len()));

    if let Some(h) = handle {
        if let Ok(sink) = Sink::try_new(h) {
            let sink_arc = Arc::new(Mutex::new(sink));
            sink_for_remove = Some(Arc::clone(&sink_arc));
            all_sinks.lock().unwrap().push(Arc::clone(&sink_arc));

            track_registry.borrow_mut().insert(track_id, TrackState {
                id: track_id, name: full_name.clone(), path: path.clone(),
                sink: Arc::clone(&sink_arc), is_playing: Arc::clone(&is_playing),
                progress: Arc::clone(&current_progress), bass_gain: Arc::clone(&bass_gain),
                volume: Rc::clone(&vol_val), speed: Rc::clone(&speed_val),
                audio_buffer: Arc::clone(&audio_arc), channels: c_count, sample_rate: s_rate, 
                total_duration_secs: Rc::clone(&dur_cell), total_samples: Rc::clone(&samples_cell),
                amplitudes: Arc::clone(&amps_arc), colors: Arc::clone(&cols_arc), effect_window: Arc::clone(&effect_window),
            });

            // PANNEAU GAUCHE
            let info_box = Box::new(Orientation::Vertical, 5);
            info_box.set_size_request(140, -1);
            info_box.set_valign(gtk::Align::Center);
            
            let header_hbox = Box::new(Orientation::Horizontal, 5);
            let label = Label::new(Some(&display_name));
            label.get_style_context().add_class("track-label");
            header_hbox.pack_start(&label, true, true, 0);
            header_hbox.pack_start(&remove_btn, false, false, 0);

            let playback_hbox = Box::new(Orientation::Horizontal, 2);
            playback_hbox.pack_start(&restart_btn, false, false, 0);
            playback_hbox.pack_start(&play_btn, false, false, 0);
            playback_hbox.pack_start(&mute_btn, false, false, 0);
            playback_hbox.pack_start(&export_btn, false, false, 0);

            let help_label = Label::new(Some("Clic G : Seek\nGlisser : Sélect.\nClic D : Annuler"));
            help_label.get_style_context().add_class("control-label");

            info_box.pack_start(&header_hbox, false, false, 0);
            info_box.pack_start(&playback_hbox, false, false, 0);
            info_box.pack_start(&help_label, false, false, 0);
            track_box.pack_start(&info_box, false, false, 5);

            // LOGIQUE BOUTONS LECTURE
            let s_mute = Arc::clone(&sink_arc);
            let v_mute = vol_val.clone();
            mute_btn.connect_clicked(move |btn| {
                let sink = s_mute.lock().unwrap();
                if sink.volume() > 0.0 { sink.set_volume(0.0); btn.set_label("UNMUTE"); } 
                else { sink.set_volume(v_mute.get() as f32); btn.set_label("M"); }
            });

            let reg_play = track_registry.clone();
            let rec_state_play = Arc::clone(recording_state);
            play_btn.connect_clicked(move |btn| {
                if let Some(track) = reg_play.borrow().get(&track_id) {
                    if btn.get_label().unwrap() == "▶" {
                        restart_playback_seamless(track, true);
                        
                        let mut rec = rec_state_play.lock().unwrap();
                        if rec.active {
                            if let Some(start) = rec.start_instant {
                                let offset = start.elapsed().as_secs_f64();
                                rec.events.push(RecordedEvent { track_id, start_offset_secs: offset, volume: track.volume.get() as f32 });
                            }
                        }
                        
                        btn.set_label("⏸");
                    } else {
                        track.sink.lock().unwrap().pause();
                        *track.is_playing.lock().unwrap() = false;
                        btn.set_label("▶");
                    }
                }
            });

            let reg_restart = track_registry.clone();
            let play_btn_clone = play_btn.clone();
            restart_btn.connect_clicked(move |_| {
                if let Some(track) = reg_restart.borrow().get(&track_id) {
                    track.sink.lock().unwrap().stop();
                    *track.progress.lock().unwrap() = 0.0;
                    *track.is_playing.lock().unwrap() = false;
                    play_btn_clone.set_label("▶");
                }
            });

            let reg_export_solo = track_registry.clone();
            export_btn.connect_clicked(move |_| {
                if let Some(track) = reg_export_solo.borrow().get(&track_id) {
                    let folder = PathBuf::from("enregistrements");
                    if !folder.exists() { let _ = fs::create_dir_all(&folder); }
                    let filename = track.path.file_stem().and_then(|s| s.to_str()).map(|s| format!("{}_modifie.wav", s)).unwrap_or("export.wav".to_string());
                    let out = folder.join(filename);
                    
                    let buf = track.audio_buffer.lock().unwrap();
                    let vol = track.volume.get() as f32;
                    let spec = hound::WavSpec { channels: track.channels, sample_rate: track.sample_rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
                    if let Ok(mut writer) = hound::WavWriter::create(&out, spec) {
                        for &sample in buf.iter() {
                            let s = (sample * vol).max(-1.0).min(1.0);
                            let _ = writer.write_sample((s * i16::MAX as f32) as i16);
                        }
                        let _ = writer.finalize();
                        println!("Piste solo exportée : {}", out.display());
                    }
                }
            });

            // PANNEAU DROIT : Onde graphique (AVEC GLISSER DEPOSER)
            let drawing_area = DrawingArea::new();
            drawing_area.set_size_request(800, 80);
            drawing_area.add_events(EventMask::POINTER_MOTION_MASK | EventMask::BUTTON_PRESS_MASK | EventMask::BUTTON_RELEASE_MASK);

            let drag_flag_press = Arc::clone(&is_dragging);
            let start_pos_press = Arc::clone(&drag_start);
            let ew_press = Arc::clone(&effect_window);
            let reg_select_ui = track_registry.clone();
            let act_select_ui = active_track_id.clone();
            let ribbon_clone_ui = ribbon_ui.clone();
            
            drawing_area.connect_button_press_event(move |da, event| {
                act_select_ui.set(Some(track_id));
                if let Some(track) = reg_select_ui.borrow().get(&track_id) { update_ribbon_from_state(&ribbon_clone_ui, track); }

                let button = event.get_button();
                let width = da.get_allocated_width() as f64;
                let (x, _) = event.get_position();
                let ratio = (x / width).clamp(0.0, 1.0);

                if button == 1 { // Clic gauche
                    *drag_flag_press.lock().unwrap() = true;
                    *start_pos_press.lock().unwrap() = ratio;
                    *ew_press.lock().unwrap() = (ratio, ratio);
                } else if button == 3 { // Clic droit
                    *ew_press.lock().unwrap() = (0.0, 1.0);
                }
                da.queue_draw();
                Inhibit(false)
            });

            let c_motion = Arc::clone(&cursor_x);
            let da_motion = drawing_area.clone();
            drawing_area.connect_motion_notify_event(move |_, event| {
                *c_motion.lock().unwrap() = event.get_position().0;
                da_motion.queue_draw();
                Inhibit(false)
            });

            let drag_flag_release = Arc::clone(&is_dragging);
            let start_pos_release = Arc::clone(&drag_start);
            let ew_release = Arc::clone(&effect_window);
            let p_release = Arc::clone(&current_progress);
            let reg_seek = track_registry.clone();
            let play_btn_seek = play_btn.clone();

            drawing_area.connect_button_release_event(move |da, event| {
                if event.get_button() == 1 {
                    *drag_flag_release.lock().unwrap() = false;
                    let width = da.get_allocated_width() as f64;
                    let (x, _) = event.get_position();
                    let ratio = (x / width).clamp(0.0, 1.0);
                    let start = *start_pos_release.lock().unwrap();
                    
                    if (ratio - start).abs() < 0.005 {
                        *ew_release.lock().unwrap() = (0.0, 1.0);
                        *p_release.lock().unwrap() = ratio;
                        
                        if let Some(track) = reg_seek.borrow().get(&track_id) {
                            restart_playback_seamless(track, false);
                            if *track.is_playing.lock().unwrap() { play_btn_seek.set_label("⏸"); }
                        }
                    }
                    da.queue_draw();
                }
                Inhibit(false)
            });

            let p_timer = Arc::clone(&current_progress);
            let playing_state = Arc::clone(&is_playing);
            let da_redraw = drawing_area.clone();
            let sc_speed = Rc::clone(&speed_val);
            let alive_flag = Rc::clone(&is_alive);
            let dur_cell_play = Rc::clone(&dur_cell);
            
            glib::timeout_add_local(100, move || {
                if !alive_flag.get() { return glib::Continue(false); }
                if *playing_state.lock().unwrap() {
                    let mut p = p_timer.lock().unwrap();
                    let dur = dur_cell_play.get();
                    if *p < 1.0 && dur > 0.0 {
                        *p += (0.1 * sc_speed.get()) / dur;
                        if *p > 1.0 { *p = 1.0; }
                        da_redraw.queue_draw();
                    }
                }
                glib::Continue(true)
            });

            let p_draw = Arc::clone(&current_progress);
            let c_draw = Arc::clone(&cursor_x);
            let amps_draw_clone = Arc::clone(&amps_arc);
            let cols_draw_clone = Arc::clone(&cols_arc);
            let ew_draw = Arc::clone(&effect_window);
            
            drawing_area.connect_draw(move |da, cr| {
                let width = da.get_allocated_width() as f64;
                let height = da.get_allocated_height() as f64;
                let mid_y = height / 2.0;
                let p = *p_draw.lock().unwrap();
                let c = *c_draw.lock().unwrap();

                cr.set_source_rgb(0.12, 0.12, 0.14);
                cr.rectangle(0.0, 0.0, width, height);
                cr.fill();

                if w.0 > 0.0 || w.1 < 1.0 {
                    cr.set_source_rgba(1.0, 0.8, 0.0, 0.25);
                    cr.rectangle(w.0 * width, 0.0, (w.1 - w.0) * width, height);
                    cr.fill();
                }

                let amps = amps_draw_clone.lock().unwrap();
                let cols = cols_draw_clone.lock().unwrap();

                for i in (0..600).step_by(4) {
                    let x = i as f64 * (width / 600.0);
                    let mut r = cols[i].0;
                    let mut g = cols[i].1;
                    let mut b = cols[i].2;

                    if x / width < p {
                        r *= 0.4; g *= 0.4; b *= 0.4;
                    }
                    
                    cr.set_source_rgb(r, g, b);
                    let amp = amps[i] as f64;
                    let h = (amp * (mid_y - 2.0)).max(1.0);
                    cr.set_line_width(2.0); cr.move_to(x, mid_y - h); cr.line_to(x, mid_y + h); cr.stroke();
                }
                
                cr.set_source_rgb(1.0, 0.2, 0.2); cr.set_line_width(2.0); cr.move_to(p * width, 0.0); cr.line_to(p * width, height); cr.stroke();
                cr.set_source_rgb(1.0, 1.0, 1.0); cr.set_line_width(1.0); cr.move_to(c, 0.0); cr.line_to(c, height); cr.stroke();
                Inhibit(false)
            });

            track_box.pack_start(&drawing_area, true, true, 5);
        }
    }

    let all_sinks_remove = Arc::clone(all_sinks);
    let eb_remove = event_box.clone();
    let container_remove = container.clone();
    let alive_remove = Rc::clone(&is_alive);
    let sink_remove = sink_for_remove.clone();

    remove_btn.connect_clicked(move |_| {
        alive_remove.set(false);
        if let Some(ref sink_arc) = sink_remove {
            let mut sinks = all_sinks_remove.lock().unwrap();
            if let Some(pos) = sinks.iter().position(|s| Arc::ptr_eq(s, sink_arc)) {
                let sink = sinks.remove(pos);
                sink.lock().unwrap().stop();
            }
        }
        container_remove.remove(&eb_remove);
    });

    container.pack_start(&event_box, false, false, 0);
    container.show_all();
}
