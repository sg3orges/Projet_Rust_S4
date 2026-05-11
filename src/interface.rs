use gtk::prelude::*;
use gtk::{
    Adjustment, FileChooserAction, FileChooserDialog, ResponseType,
    Window, WindowType, Box, Orientation, Button, Label,
    ScrolledWindow, Toolbar, ToolButton, Settings, DrawingArea, Scale,
    CssProvider, StyleContext
};
use gdk::EventMask;
use glib; 
use std::path::PathBuf;
use std::fs::File;
use std::io::BufReader;
use rodio::{Decoder, OutputStream, Sink, OutputStreamHandle, Source};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, AtomicBool, Ordering};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;
use hound;
use std::error::Error;
use std::fs;
use std::time::Instant;
use gdk::Screen;


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
#[derive(Clone, Debug)]
struct RecordedEvent {
    path: PathBuf,
    start_offset_secs: f64,
    bass_gain: f32,
    disto_on: bool,
    reverb_on: bool,
    volume: f32,
}

#[derive(Debug)]
struct RecordingState {
    active: bool,
    start_instant: Option<Instant>,
    session_duration_secs: f64,
    events: Vec<RecordedEvent>,
}

impl RecordingState 
{
    fn new() -> Self 
    {
        Self 
        {
            active: false,
            start_instant: None,
            session_duration_secs: 0.0,
            events: Vec::new(),
        }
    }
}
fn load_recording_css() {
    let provider = CssProvider::new();

    provider
        .load_from_data(b"#rec-start-recording.recording 
        {
            background: #c62828;
            color: white;
            border-radius: 6px;
            font-weight: bold;
        }

        #rec-start-recording.recording label 
        {
            color: white;
            font-weight: bold;
        }",).expect("Impossible de charger le CSS");

    if let Some(screen) = Screen::get_default() 
    {
        StyleContext::add_provider_for_screen(&screen, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,);
    }
}
fn start_recording_session(recording_state: &Arc<Mutex<RecordingState>>) {
    let mut state = recording_state.lock().unwrap();
    state.active = true;
    state.start_instant = Some(Instant::now());
    state.events.clear();
    println!("Enregistrement démarré");
}
fn stop_recording_session(recording_state: &Arc<Mutex<RecordingState>>) 
{
    let mut state = recording_state.lock().unwrap();

    if let Some(start) = state.start_instant {
        state.session_duration_secs = start.elapsed().as_secs_f64();
    } else {
        state.session_duration_secs = 0.0;
    }

    state.active = false;
    state.start_instant = None;

    println!(
        "Enregistrement arrêté (durée session = {:.2}s)",
        state.session_duration_secs
    );
}
fn get_session_duration(recording_state: &Arc<Mutex<RecordingState>>) -> f64 
{
    let state = recording_state.lock().unwrap();
    state.session_duration_secs
}
fn record_play_event(
    recording_state: &Arc<Mutex<RecordingState>>,
    path: &PathBuf,
    bass_gain: f32,
    disto_on: bool,
    reverb_on: bool,
    volume: f32,
) {
    let mut state = recording_state.lock().unwrap();

    if !state.active {
        return;
    }

    let start = match state.start_instant {
        Some(start) => start,
        None => return,
    };

    let offset = start.elapsed().as_secs_f64();

    state.events.push(RecordedEvent {
        path: path.clone(),
        start_offset_secs: offset,
        bass_gain,
        disto_on,
        reverb_on,
        volume,
    });

    println!(
        "Event enregistré : {} à {:.2}s",
        path.display(),
        offset
    );
}
fn get_recorded_events(recording_state: &Arc<Mutex<RecordingState>>) -> Vec<RecordedEvent> {
    let state = recording_state.lock().unwrap();
    state.events.clone()
}
fn render_track_with_effects(
    input_path: &PathBuf,
    bass_gain: f32,
    disto_on: bool,
    reverb_on: bool,
    volume: f32,
) -> Result<(Vec<f32>, u16, u32), std::boxed::Box<dyn std::error::Error>> {
    let f = File::open(input_path)?;
    let decoder = Decoder::new(BufReader::new(f))?;

    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();

    let source = decoder.convert_samples::<f32>();

    let filtered = DSPFilter {
        input: source,
        bass_gain: Arc::new(AtomicU32::new(bass_gain.to_bits())),
        prev_low: 0.0,
        disto_on: Arc::new(AtomicBool::new(disto_on)),
        reverb_on: Arc::new(AtomicBool::new(reverb_on)),
        reverb_buffer: vec![0.0; 8000],
        reverb_index: 0,
    };

    let mut samples = Vec::new();

    for sample in filtered {
        samples.push((sample * volume).max(-1.0).min(1.0));
    }

    Ok((samples, channels, sample_rate))
}
fn write_wav_file(
    output_path: &PathBuf,
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
) -> Result<(), std::boxed::Box<dyn std::error::Error>> {
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = hound::WavWriter::create(output_path, spec)?;

    for &sample in samples {
        let s = sample.max(-1.0).min(1.0);
        let s_i16 = (s * i16::MAX as f32) as i16;
        writer.write_sample(s_i16)?;
    }

    writer.finalize()?;
    Ok(())
}
fn export_recorded_session(
    events: &[RecordedEvent],
    session_duration_secs: f64,
    output_path: &PathBuf,
) -> Result<(), std::boxed::Box<dyn std::error::Error>> {
    if events.is_empty() {
        return Err("Aucun événement enregistré".into());
    }

    let mut rendered_tracks: Vec<(Vec<f32>, u16, u32, usize)> = Vec::new();

    let mut reference_channels: Option<u16> = None;
    let mut reference_sample_rate: Option<u32> = None;
    let mut total_len: usize = 0;

    for event in events {
        let (mut samples, channels, sample_rate) = render_track_with_effects(
            &event.path,
            event.bass_gain,
            event.disto_on,
            event.reverb_on,
            event.volume,
        )?;

        if reference_channels.is_none() 
        {
            reference_channels = Some(channels);
        }

        if reference_sample_rate.is_none() 
        {
            reference_sample_rate = Some(sample_rate);
        }

        if Some(channels) != reference_channels || Some(sample_rate) != reference_sample_rate {
            return Err("Toutes les pistes doivent avoir le même sample_rate et le même nombre de canaux".into(),);
        }

        let channels_usize = channels as usize;

        let remaining_secs = session_duration_secs - event.start_offset_secs;
        if remaining_secs <= 0.0 
        {
            continue;
        }

        let max_samples_for_session =
            (remaining_secs * sample_rate as f64 * channels as f64) as usize;

        let aligned_max_samples =
            (max_samples_for_session / channels_usize) * channels_usize;

        if samples.len() > aligned_max_samples 
        {
            samples.truncate(aligned_max_samples);
        }

        let raw_offset_samples =
            (event.start_offset_secs * sample_rate as f64 * channels as f64) as usize;

        let offset_samples =
            (raw_offset_samples / channels_usize) * channels_usize;

        let end_len = offset_samples + samples.len();
        if end_len > total_len 
        {
            total_len = end_len;
        }

        rendered_tracks.push((samples, channels, sample_rate, offset_samples));
    }

    if rendered_tracks.is_empty() 
    {
        return Err("Aucune piste exploitable pour l'export".into());
    }

    let channels = reference_channels.unwrap();
    let sample_rate = reference_sample_rate.unwrap();
    let channels_usize = channels as usize;
    total_len = (total_len / channels_usize) * channels_usize;

    let mut mix_buffer = vec![0.0f32; total_len];

    for (samples, _channels, _sample_rate, offset_samples) in rendered_tracks {
        for (i, sample) in samples.iter().enumerate() {
            let pos = offset_samples + i;
            if pos < mix_buffer.len() {
                mix_buffer[pos] += *sample;
            }
        }
    }

    for s in mix_buffer.iter_mut() {
        *s = s.max(-1.0).min(1.0);
    }

    write_wav_file(output_path, &mix_buffer, channels, sample_rate)?;
    Ok(())
}
fn export_recorded_session_to_default_folder(
    recording_state: &Arc<Mutex<RecordingState>>,
) -> Result<PathBuf, std::boxed::Box<dyn std::error::Error>> 
{
    let events = get_recorded_events(recording_state);
    let session_duration_secs = get_session_duration(recording_state);

    if events.is_empty() {
        return Err("Aucun événement enregistré".into());
    }

    let folder = PathBuf::from("enregistrements");
    if !folder.exists() {
        fs::create_dir_all(&folder)?;
    }

    let output_path = folder.join("mix_session.wav");

    export_recorded_session(&events, session_duration_secs, &output_path)?;

    Ok(output_path)
}
fn print_recorded_events(recording_state: &Arc<Mutex<RecordingState>>) {
    let state = recording_state.lock().unwrap();

    println!("--- EVENTS ENREGISTRES ---");
    for (i, event) in state.events.iter().enumerate() {
        println!(
            "{} | {} | start={:.2}s | bass={:.2} | disto={} | reverb={} | vol={:.2}",
            i + 1,
            event.path.display(),
            event.start_offset_secs,
            event.bass_gain,
            event.disto_on,
            event.reverb_on,
            event.volume
        );
    }
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
    load_recording_css();

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

    let recording_state = Arc::new(Mutex::new(RecordingState::new()));

    let main_vbox = Box::new(Orientation::Vertical, 0);
    window.add(&main_vbox);

    let toolbar = Toolbar::new();
    let add_track_btn = ToolButton::new::<Button>(None, Some("Ajouter Piste"));
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

    // --- LE RUBAN ---
    let ribbon_panel = Box::new(Orientation::Vertical, 5);
    ribbon_panel.get_style_context().add_class("ribbon-panel");
    
    let ribbon_header = Label::new(Some("Veuillez sÃ©lectionner une piste en cliquant sur son graphique..."));
    ribbon_header.get_style_context().add_class("track-label");
    ribbon_panel.pack_start(&ribbon_header, false, false, 5);

    let ribbon_controls = Box::new(Orientation::Horizontal, 20);
    ribbon_controls.set_halign(gtk::Align::Center);

    let mk_scale_box = |name: &str, scale: &Scale| -> Box {
        let vbox = Box::new(Orientation::Vertical, 2);
        let l = Label::new(Some(name));
        l.get_style_context().add_class("control-label");
        vbox.pack_start(&l, false, false, 0);
        vbox.pack_start(scale, false, false, 0);
        vbox
    };

    let r_scale_vol = Scale::with_range(Orientation::Horizontal, 0.0, 2.0, 0.1);
    r_scale_vol.set_size_request(100, -1);
    let r_scale_speed = Scale::with_range(Orientation::Horizontal, 0.25, 2.0, 0.05);
    r_scale_speed.set_size_request(100, -1);
    let r_scale_bass = Scale::with_range(Orientation::Horizontal, 0.0, 3.0, 0.1);
    r_scale_bass.set_size_request(100, -1);

    // BOUTONS D'EFFETS
    let r_btn_apply_vol = Button::with_label("Appliquer Volume");
    r_btn_apply_vol.get_style_context().add_class("btn-vol");
    
    let r_btn_apply_speed = Button::with_label("Appliquer Vitesse");
    r_btn_apply_speed.get_style_context().add_class("btn-speed");

    let r_btn_apply_bass = Button::with_label("Appliquer Basses");
    r_btn_apply_bass.get_style_context().add_class("btn-bass");
    
    let r_btn_apply_disto = Button::with_label("Appliquer Distorsion");
    r_btn_apply_disto.get_style_context().add_class("btn-disto");
    
    let r_btn_apply_reverb = Button::with_label("Appliquer Reverb");
    r_btn_apply_reverb.get_style_context().add_class("btn-reverb");

    // GROUPES DU RUBAN
    let vol_group = Box::new(Orientation::Vertical, 2);
    vol_group.pack_start(&mk_scale_box("Multiplicateur Volume", &r_scale_vol), false, false, 0);
    vol_group.pack_start(&r_btn_apply_vol, false, false, 0);

    let speed_group = Box::new(Orientation::Vertical, 2);
    speed_group.pack_start(&mk_scale_box("Facteur Vitesse", &r_scale_speed), false, false, 0);
    speed_group.pack_start(&r_btn_apply_speed, false, false, 0);

    let bass_group = Box::new(Orientation::Vertical, 2);
    bass_group.pack_start(&mk_scale_box("Niveau Basses", &r_scale_bass), false, false, 0);
    bass_group.pack_start(&r_btn_apply_bass, false, false, 0);

    let fx_group = Box::new(Orientation::Vertical, 5);
    fx_group.set_valign(gtk::Align::End);
    fx_group.pack_start(&r_btn_apply_disto, false, false, 0);
    fx_group.pack_start(&r_btn_apply_reverb, false, false, 0);

    ribbon_controls.pack_start(&vol_group, false, false, 0);
    ribbon_controls.pack_start(&speed_group, false, false, 0);
    ribbon_controls.pack_start(&bass_group, false, false, 0);
    ribbon_controls.pack_start(&fx_group, false, false, 0);
    
    ribbon_panel.pack_start(&ribbon_controls, false, false, 5);
    main_vbox.pack_start(&ribbon_panel, false, false, 0);

    // --- ZONE DES PISTES ---
    let scroll = ScrolledWindow::new(None::<&Adjustment>, None::<&Adjustment>);
    let track_container = Box::new(Orientation::Vertical, 10);
    track_container.set_margin_top(15);
    track_container.set_margin_bottom(15);
    track_container.set_margin_start(15);
    track_container.set_margin_end(15);
    scroll.add(&track_container);
    main_vbox.pack_start(&scroll, true, true, 0);

    let sinks_for_play = Arc::clone(&all_sinks);
    play_all_btn.connect_clicked(move |_| {
        for s in sinks_for_play.lock().unwrap().iter() {
            s.lock().unwrap().play();
        }
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

    let rec_state_start = Arc::clone(&recording_state);
    let rec_start_btn_clone = rec_start_btn.clone();
    rec_start_btn.connect_clicked(move |_| {
    start_recording_session(&rec_state_start);

    let style_context = rec_start_btn_clone.get_style_context();
    style_context.add_class("recording");

    rec_start_btn_clone.set_label(Some("REC ●"));
});

    let rec_state_stop = Arc::clone(&recording_state);
    let rec_start_btn_stop = rec_start_btn.clone();
    rec_stop_btn.connect_clicked(move |_| {
    stop_recording_session(&rec_state_stop);

    let style_context = rec_start_btn_stop.get_style_context();
    style_context.remove_class("recording");

    rec_start_btn_stop.set_label(Some("REC START"));

    print_recorded_events(&rec_state_stop);

    match export_recorded_session_to_default_folder(&rec_state_stop) {
        Ok(path) => println!("Mix exporté : {}", path.display()),
        Err(e) => eprintln!("Erreur export session : {}", e),
    }
});

    let window_clone = window.clone();
    let handle_clone = handle.clone();
    let sinks_for_add = Arc::clone(&all_sinks);
    let rec_state_add = Arc::clone(&recording_state);

    add_track_btn.connect_clicked(move |_| {
        let dialog = FileChooserDialog::with_buttons(
            Some("Select Audio"),
            Some(&window_clone),
            FileChooserAction::Open,
            &[("_Cancel", ResponseType::Cancel), ("_Open", ResponseType::Accept)],
        );

        if dialog.run() == ResponseType::Accept {
            if let Some(filename) = dialog.get_filename() {
                create_track_row(
                    &track_container,
                    filename,
                    handle_clone.as_ref(),
                    &sinks_for_add,
                    &rec_state_add,
                );
            }
        }

        dialog.close();
    });

    window.connect_delete_event(|_, _| {
        gtk::main_quit();
        Inhibit(false)
    });

    window
}

fn create_track_row(
    container: &Box,
    path: PathBuf,
    handle: Option<&Arc<OutputStreamHandle>>,
    all_sinks: &Arc<Mutex<Vec<Arc<Mutex<Sink>>>>>,
    recording_state: &Arc<Mutex<RecordingState>>,
) {
    let track_box = Box::new(Orientation::Horizontal, 10);
    let is_alive = Rc::new(Cell::new(true));
    let mut sink_for_remove: Option<Arc<Mutex<Sink>>> = None;

    let restart_btn = Button::with_label("⏮");
    let play_btn = Button::with_label("▶");
    let mute_btn = Button::with_label("M");
    let save_btn = Button::with_label("Enregistrer");
    let remove_btn = Button::with_label("✖");
    remove_btn.set_tooltip_text(Some("Supprimer cette piste"));

    let vol_scale = Scale::with_range(Orientation::Horizontal, 0.0, 2.0, 0.1);
    vol_scale.set_value(1.0);
    vol_scale.set_size_request(70, -1);

    let speed_scale = Scale::with_range(Orientation::Horizontal, 0.25, 2.0, 0.05);
    speed_scale.set_value(1.0);
    speed_scale.set_size_request(70, -1);

    let bass_scale = Scale::with_range(Orientation::Horizontal, 0.0, 3.0, 0.1);
    bass_scale.set_value(1.0);
    bass_scale.set_size_request(70, -1);

    let disto_btn = Button::with_label("Distorsion");
    let reverb_btn = Button::with_label("Reverb");

    let bass_gain = Arc::new(AtomicU32::new(1f32.to_bits()));
    let disto_on = Arc::new(AtomicBool::new(false));
    let reverb_on = Arc::new(AtomicBool::new(false));

    let cursor_x = Arc::new(Mutex::new(0.0));
    let current_progress = Arc::new(Mutex::new(0.0));
    let is_playing = Arc::new(Mutex::new(false));

    let mut amplitudes = Vec::new();
    let mut total_duration_secs = 1.0;

    if let Ok(f) = File::open(&path) {
        if let Ok(d) = Decoder::new(BufReader::new(f)) {
            let channels = d.channels() as f64;
            let sample_rate = d.sample_rate() as f64;

            let samples: Vec<f32> = d.convert_samples::<f32>().collect();

            if sample_rate > 0.0 && channels > 0.0 {
                total_duration_secs = samples.len() as f64 / (sample_rate * channels);
            }

            let num_points = 600;
            let chunk_size = (samples.len() / num_points).max(1);
            let mut max_overall_amp: f32 = 0.001;

            for chunk in samples.chunks(chunk_size) {
                let mut local_max = 0.0f32;
                for &s in chunk {
                    let abs_s = s.abs();
                    if abs_s > local_max {
                        local_max = abs_s;
                    }
                }
                if local_max > max_overall_amp {
                    max_overall_amp = local_max;
                }
                amplitudes.push(local_max);
                if amplitudes.len() == num_points {
                    break;
                }
            }

            for a in amplitudes.iter_mut() {
                *a /= max_overall_amp;
            }
        }
    }

    if amplitudes.is_empty() {
        amplitudes.resize(600, 0.0);
    }

    let amps_arc = Arc::new(amplitudes);

    if let Some(h) = handle {
        if let Ok(sink) = Sink::try_new(h) {
            let sink_arc = Arc::new(Mutex::new(sink));
            sink_for_remove = Some(Arc::clone(&sink_arc));
            all_sinks.lock().unwrap().push(Arc::clone(&sink_arc));

            let drawing_area = DrawingArea::new();
            drawing_area.set_size_request(500, 80);
            drawing_area.add_events(EventMask::POINTER_MOTION_MASK);

            let s_vol = Arc::clone(&sink_arc);
            vol_scale.connect_value_changed(move |sc| {
                s_vol.lock().unwrap().set_volume(sc.get_value() as f32);
            });

            let s_speed = Arc::clone(&sink_arc);
            speed_scale.connect_value_changed(move |sc| {
                s_speed.lock().unwrap().set_speed(sc.get_value() as f32);
            });

            let bg_clone = Arc::clone(&bass_gain);
            bass_scale.connect_value_changed(move |sc| {
                bg_clone.store((sc.get_value() as f32).to_bits(), Ordering::Relaxed);
            });

            let d_state = Arc::clone(&disto_on);
            disto_btn.connect_clicked(move |btn| {
                let current = d_state.load(Ordering::Relaxed);
                d_state.store(!current, Ordering::Relaxed);
                if !current {
                    btn.set_label("Distorsion: ON");
                } else {
                    btn.set_label("Distorsion");
                }
            });

            let r_state = Arc::clone(&reverb_on);
            reverb_btn.connect_clicked(move |btn| {
                let current = r_state.load(Ordering::Relaxed);
                r_state.store(!current, Ordering::Relaxed);
                if !current {
                    btn.set_label("Reverb: ON");
                } else {
                    btn.set_label("Reverb");
                }
            });

            let s_mute = Arc::clone(&sink_arc);
            let v_scale_mute = vol_scale.clone();
            mute_btn.connect_clicked(move |btn| {
                let sink = s_mute.lock().unwrap();
                if sink.volume() > 0.0 {
                    sink.set_volume(0.0);
                    btn.set_label("UNMUTE");
                } else {
                    sink.set_volume(v_scale_mute.get_value() as f32);
                    btn.set_label("M");
                }
            });

            let c_motion = Arc::clone(&cursor_x);
            let da_motion = drawing_area.clone();
            let drag_flag_motion = Arc::clone(&is_dragging);
            let start_pos_motion = Arc::clone(&drag_start);
            let ew_motion = Arc::clone(&effect_window);
            drawing_area.connect_motion_notify_event(move |da, event| {
                let width = da.get_allocated_width() as f64;
                let (x, _) = event.get_position();
                let ratio = (x / width).clamp(0.0, 1.0);
                *c_motion.lock().unwrap() = x;

                if *drag_flag_motion.lock().unwrap() {
                    let start = *start_pos_motion.lock().unwrap();
                    let mut w = ew_motion.lock().unwrap();
                    w.0 = start.min(ratio);
                    w.1 = start.max(ratio);
                }
                da_motion.queue_draw();
                Inhibit(false)
            });

            let p_timer = Arc::clone(&current_progress);
            let playing_state = Arc::clone(&is_playing);
            let da_redraw = drawing_area.clone();
            let sc_speed_timer = speed_scale.clone();
            let alive_flag = Rc::clone(&is_alive);
            glib::timeout_add_local(100, move || {
                if !alive_flag.get() {
                    return glib::Continue(false);
                }
                if *playing_state.lock().unwrap() {
                    let mut p = p_timer.lock().unwrap();
                    if *p < 1.0 {
                        let elapsed_time = 0.1 * sc_speed_timer.get_value() as f64;
                        *p += elapsed_time / total_duration_secs;

                        if *p >= 1.0 {
                            *p = 1.0;
                        }
                        da_redraw.queue_draw();
                    }
                }
                glib::Continue(true)
            });

            let p_draw = Arc::clone(&current_progress);
            let c_draw = Arc::clone(&cursor_x);
            let amps_draw = Arc::clone(&amps_arc);

            drawing_area.connect_draw(move |da, cr| {
                let width = da.get_allocated_width() as f64;
                let mid_y = 40.0;
                let p = *p_draw.lock().unwrap();
                let c = *c_draw.lock().unwrap();
                let w = *ew_draw.lock().unwrap();

                for i in (0..600).step_by(4) {
                    let x = i as f64 * (width / 600.0);

                    if x / width < p {
                        cr.set_source_rgb(0.0, 0.8, 0.2);
                    } else {
                        cr.set_source_rgb(0.2, 0.6, 1.0);
                    }

                    let amp = if i < amps_draw.len() {
                        amps_draw[i] as f64
                    } else {
                        0.0
                    };

                    let h = (amp * 38.0).max(1.0);

                    cr.set_line_width(2.0);
                    cr.move_to(x, mid_y - h);
                    cr.line_to(x, mid_y + h);
                    cr.stroke();
                }

                cr.set_source_rgb(1.0, 0.2, 0.2);
                cr.set_line_width(2.0);
                cr.move_to(p * width, 0.0);
                cr.line_to(p * width, 80.0);
                cr.stroke();

                cr.set_source_rgb(1.0, 1.0, 1.0);
                cr.set_line_width(1.0);
                cr.move_to(c, 0.0);
                cr.line_to(c, 80.0);
                cr.stroke();

                Inhibit(false)
            });

            let file_path = path.to_str().unwrap().to_string();
            let s_play = Arc::clone(&sink_arc);
            let p_state = Arc::clone(&is_playing);
            let bg_play = Arc::clone(&bass_gain);
            let dist_play = Arc::clone(&disto_on);
            let rev_play = Arc::clone(&reverb_on);
            let path_record = path.clone();
            let rec_state_play = Arc::clone(recording_state);
            let vol_scale_play = vol_scale.clone();

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

                    let bass = f32::from_bits(bg_play.load(Ordering::Relaxed));
                    let dist = dist_play.load(Ordering::Relaxed);
                    let rev = rev_play.load(Ordering::Relaxed);
                    let vol = vol_scale_play.get_value() as f32;

                    record_play_event(
                        &rec_state_play,
                        &path_record,
                        bass,
                        dist,
                        rev,
                        vol,
                    );

                    sink.play();
                    *playing = true;
                    btn.set_label("⏸");
                } else {
                    sink.pause();
                    *playing = false;
                    btn.set_label("▶");
                }
            });

            let s_restart = Arc::clone(&sink_arc);
            let p_restart = Arc::clone(&current_progress);
            let play_btn_restart = play_btn.clone();
            let path_restart = path.clone();
            let bg_restart = Arc::clone(&bass_gain);
            let dist_restart = Arc::clone(&disto_on);
            let rev_restart = Arc::clone(&reverb_on);
            let is_playing_restart = Arc::clone(&is_playing);
            let drawing_restart = drawing_area.clone();

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
                drawing_restart.queue_draw();
            });

            let path_save = path.clone();
            let bg_save = Arc::clone(&bass_gain);
            let dist_save = Arc::clone(&disto_on);
            let rev_save = Arc::clone(&reverb_on);
            let vol_save = vol_scale.clone();

            save_btn.connect_clicked(move |_| {
                let folder = PathBuf::from("enregistrements");

                if !folder.exists() {
                    if let Err(e) = fs::create_dir_all(&folder) {
                        eprintln!("Erreur création dossier : {}", e);
                        return;
                    }
                }

                let filename = path_save
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| format!("{}_modifie.wav", s))
                    .unwrap_or("export_modifie.wav".to_string());

                let output_path = folder.join(filename);

                let bass = f32::from_bits(bg_save.load(Ordering::Relaxed));
                let dist = dist_save.load(Ordering::Relaxed);
                let rev = rev_save.load(Ordering::Relaxed);
                let vol = vol_save.get_value() as f32;

                match export_with_effects(&path_save, &output_path, bass, dist, rev, vol) {
                    Ok(_) => println!("Export réussi : {}", output_path.display()),
                    Err(e) => eprintln!("Erreur export : {}", e),
                }
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
            track_box.pack_start(&save_btn, false, false, 2);
            track_box.pack_start(&drawing_area, true, true, 5);

            let reg_select = track_registry.clone();
            let act_select = active_track_id.clone();
            let ribbon_clone = ribbon_ui.clone();
            event_box.connect_button_press_event(move |_, _| {
                act_select.set(Some(track_id));
                if let Some(track) = reg_select.borrow().get(&track_id) {
                    update_ribbon_from_state(&ribbon_clone, track);
                }
                Inhibit(false)
            });
        }
    }

    let all_sinks_remove = Arc::clone(all_sinks);
    let track_box_remove = track_box.clone();
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

        container_remove.remove(&track_box_remove);
    });

    let label = Label::new(Some(
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("audio"),
    ));
    track_box.pack_start(&remove_btn, false, false, 2);
    track_box.pack_start(&label, false, false, 5);

    container.pack_start(&track_box, false, false, 5);
    container.show_all();
}

fn export_with_effects(
    input_path: &PathBuf,
    output_path: &PathBuf,
    bass_gain: f32,
    disto_on: bool,
    reverb_on: bool,
    volume: f32,
) -> Result<(), std::boxed::Box<dyn Error>> {
    let f = File::open(input_path)?;
    let decoder = Decoder::new(BufReader::new(f))?;

    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();

    let source = decoder.convert_samples::<f32>();

    let filtered = DSPFilter {
        input: source,
        bass_gain: Arc::new(AtomicU32::new(bass_gain.to_bits())),
        prev_low: 0.0,
        disto_on: Arc::new(AtomicBool::new(disto_on)),
        reverb_on: Arc::new(AtomicBool::new(reverb_on)),
        reverb_buffer: vec![0.0; 8000],
        reverb_index: 0,
    };

    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = hound::WavWriter::create(output_path, spec)?;

    for sample in filtered {
        let s = (sample * volume).max(-1.0).min(1.0);
        let s_i16 = (s * i16::MAX as f32) as i16;
        writer.write_sample(s_i16)?;
    }

    writer.finalize()?;
    Ok(())
}
