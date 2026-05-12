use gtk::prelude::*;
use gtk::{
    Adjustment, Box as GtkBox, Button, CssProvider, DrawingArea, FileChooserAction,
    FileChooserDialog, Label, Orientation, ResponseType, Scale, ScrolledWindow, Settings,
    StyleContext, Toolbar, ToolButton, Window, WindowType,
};
use gdk::{EventMask, Screen};
use glib;
use hound;
use rodio::{buffer::SamplesBuffer, Decoder, OutputStream, Sink, Source};
use std::cell::RefCell;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

const TRACK_HEIGHT: f64 = 86.0;
const HEADER_WIDTH: f64 = 170.0;
const RULER_HEIGHT: f64 = 28.0;
const CLIP_MARGIN_Y: f64 = 12.0;
const DEFAULT_PIXELS_PER_SECOND: f64 = 90.0;

#[derive(Clone, Debug)]
struct AudioClip {
    id: u32,
    lane: usize,
    name: String,
    path: PathBuf,
    buffer: Vec<f32>,
    amplitudes: Vec<f32>,
    channels: u16,
    sample_rate: u32,
    start_time_secs: f64,
    offset_in_audio_secs: f64,
    duration_secs: f64,
    volume: f32,
    speed: f64,
    bass_gain: f32,
    selection: Option<(f64, f64)>,
    color: (f64, f64, f64),
    effect_zones: Vec<(f64, f64, (f64, f64, f64))>,
}

impl AudioClip {
    fn end_time_secs(&self) -> f64 {
        self.start_time_secs + self.duration_secs
    }

    fn refresh_duration(&mut self) {
        self.duration_secs =
            self.buffer.len() as f64 / (self.sample_rate as f64 * self.channels as f64);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MouseMode {
    None,
    MoveClip,
    SelectInClip,
}

#[derive(Debug)]
struct TimelineState {
    clips: Vec<AudioClip>,
    lanes: usize,
    next_clip_id: u32,
    pixels_per_second: f64,
    playhead_secs: f64,
    is_playing: bool,
    selected_clip_id: Option<u32>,
    mouse_mode: MouseMode,
    drag_clip_id: Option<u32>,
    drag_offset_secs: f64,
    selection_start_ratio: f64,
    last_tick: Option<Instant>,
}

impl TimelineState {
    fn new() -> Self {
        Self {
            clips: Vec::new(),
            lanes: 3,
            next_clip_id: 1,
            pixels_per_second: DEFAULT_PIXELS_PER_SECOND,
            playhead_secs: 0.0,
            is_playing: false,
            selected_clip_id: None,
            mouse_mode: MouseMode::None,
            drag_clip_id: None,
            drag_offset_secs: 0.0,
            selection_start_ratio: 0.0,
            last_tick: None,
        }
    }

    fn timeline_duration_secs(&self) -> f64 {
        self.clips
            .iter()
            .map(|clip| clip.end_time_secs())
            .fold(30.0, f64::max)
    }

    fn add_clip_from_file(&mut self, path: PathBuf) -> Result<(), String> {
        let file = File::open(&path).map_err(|e| format!("Impossible d'ouvrir le fichier: {e}"))?;
        let decoder = Decoder::new(BufReader::new(file)).map_err(|e| format!("Audio invalide: {e}"))?;

        let channels = decoder.channels();
        let sample_rate = decoder.sample_rate();
        let buffer: Vec<f32> = decoder.convert_samples::<f32>().collect();

        if buffer.is_empty() || channels == 0 || sample_rate == 0 {
            return Err("Fichier audio vide ou invalide".to_string());
        }

        let duration_secs = buffer.len() as f64 / (sample_rate as f64 * channels as f64);
        let amplitudes = compute_amplitudes(&buffer, 900);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("clip")
            .to_string();

        let lane = self.first_free_lane_at_playhead(duration_secs);
        self.lanes = self.lanes.max(lane + 1);

        let colors = [
            (0.20, 0.74, 0.95),
            (1.00, 0.55, 0.18),
            (0.60, 0.82, 0.20),
            (0.94, 0.27, 0.38),
            (0.70, 0.45, 0.95),
            (0.95, 0.80, 0.20),
        ];

        self.clips.push(AudioClip {
            id: self.next_clip_id,
            lane,
            name,
            path,
            buffer,
            amplitudes,
            channels,
            sample_rate,
            start_time_secs: self.playhead_secs,
            offset_in_audio_secs: 0.0,
            duration_secs,
            volume: 1.0,
            speed: 1.0,
            bass_gain: 1.0,
            selection: None,
            color: colors[(self.next_clip_id as usize - 1) % colors.len()],
            effect_zones: Vec::new(),
        });

        self.selected_clip_id = Some(self.next_clip_id);
        self.next_clip_id += 1;
        Ok(())
    }

    fn first_free_lane_at_playhead(&self, duration_secs: f64) -> usize {
        let start = self.playhead_secs;
        let end = start + duration_secs;

        for lane in 0..self.lanes {
            let occupied = self.clips.iter().any(|clip| {
                clip.lane == lane && start < clip.end_time_secs() && end > clip.start_time_secs
            });
            if !occupied {
                return lane;
            }
        }
        self.lanes
    }

    fn hit_test_clip(&self, x: f64, y: f64) -> Option<u32> {
        if x < HEADER_WIDTH || y < RULER_HEIGHT {
            return None;
        }

        let time = (x - HEADER_WIDTH) / self.pixels_per_second;
        let lane = ((y - RULER_HEIGHT) / TRACK_HEIGHT).floor() as usize;

        self.clips
            .iter()
            .rev()
            .find(|clip| {
                clip.lane == lane
                    && time >= clip.start_time_secs
                    && time <= clip.end_time_secs()
            })
            .map(|clip| clip.id)
    }

    fn clip_ratio_at_mouse(&self, clip_id: u32, mouse_x: f64) -> Option<f64> {
        let clip = self.clips.iter().find(|clip| clip.id == clip_id)?;
        let time = (mouse_x - HEADER_WIDTH) / self.pixels_per_second;
        Some(((time - clip.start_time_secs) / clip.duration_secs).clamp(0.0, 1.0))
    }

    fn set_clip_position_from_mouse(&mut self, clip_id: u32, mouse_x: f64, mouse_y: f64) {
        let mut new_start = ((mouse_x - HEADER_WIDTH) / self.pixels_per_second) - self.drag_offset_secs;
        new_start = snap_time(new_start.max(0.0), 0.25);

        let mut new_lane = if mouse_y > RULER_HEIGHT {
            ((mouse_y - RULER_HEIGHT) / TRACK_HEIGHT).floor() as usize
        } else {
            0
        };
        new_lane = new_lane.min(63);
        self.lanes = self.lanes.max(new_lane + 1);

        if let Some(clip) = self.clips.iter_mut().find(|clip| clip.id == clip_id) {
            clip.start_time_secs = new_start;
            clip.lane = new_lane;
        }
    }

    fn selected_clip_mut(&mut self) -> Option<&mut AudioClip> {
        let id = self.selected_clip_id?;
        self.clips.iter_mut().find(|clip| clip.id == id)
    }

    fn selected_clip(&self) -> Option<&AudioClip> {
        let id = self.selected_clip_id?;
        self.clips.iter().find(|clip| clip.id == id)
    }
}

fn snap_time(value: f64, grid: f64) -> f64 {
    (value / grid).round() * grid
}

fn compute_amplitudes(samples: &[f32], count: usize) -> Vec<f32> {
    if samples.is_empty() {
        return vec![0.0; count];
    }

    let chunk_size = (samples.len() / count).max(1);
    let mut amps = vec![0.0; count];
    let mut max_amp = 0.001f32;

    for (i, chunk) in samples.chunks(chunk_size).take(count).enumerate() {
        let local_max = chunk.iter().fold(0.0f32, |m, sample| m.max(sample.abs()));
        amps[i] = local_max;
        max_amp = max_amp.max(local_max);
    }

    for amp in &mut amps {
        *amp /= max_amp;
    }

    amps
}

fn selection_sample_range(clip: &AudioClip) -> Option<(usize, usize)> {
    let (a, b) = clip.selection?;
    if (b - a).abs() < 0.001 {
        return None;
    }

    let total = clip.buffer.len();
    let channels = clip.channels as usize;
    if total == 0 || channels == 0 {
        return None;
    }

    let mut start = (a.min(b) * total as f64) as usize;
    let mut end = (a.max(b) * total as f64) as usize;
    start = (start / channels) * channels;
    end = (end / channels) * channels;
    end = end.min(total);

    if start >= end {
        return None;
    }
    Some((start, end))
}

fn selected_or_full_range(clip: &AudioClip) -> Option<(usize, usize)> {
    selection_sample_range(clip).or_else(|| {
        let channels = clip.channels as usize;
        let end = (clip.buffer.len() / channels) * channels;
        if end > 0 {
            Some((0, end))
        } else {
            None
        }
    })
}

fn add_effect_zone(clip: &mut AudioClip, color: (f64, f64, f64)) {
    let zone = clip.selection.unwrap_or((0.0, 1.0));
    clip.effect_zones.push((zone.0, zone.1, color));
}

fn apply_effect_to_clip(clip: &mut AudioClip, effect: &str) {
    let Some((start, end)) = selected_or_full_range(clip) else {
        return;
    };

    let channels = clip.channels as usize;
    if channels == 0 {
        return;
    }

    if effect == "disto" {
        for i in start..end {
            clip.buffer[i] = (clip.buffer[i] * 5.0).tanh() * 0.7;
        }
        add_effect_zone(clip, (1.0, 0.55, 0.0));
    } else if effect == "reverb" {
        let mut delay_buf = vec![0.0; 8000];
        let mut idx = 0;
        for i in start..end {
            let sample = clip.buffer[i];
            let delayed = delay_buf[idx];
            delay_buf[idx] = sample + delayed * 0.4;
            idx = (idx + 1) % 8000;
            clip.buffer[i] = ((sample * 0.7) + (delayed * 0.5)).clamp(-1.0, 1.0);
        }
        add_effect_zone(clip, (0.61, 0.15, 0.69));
    } else if effect == "bass" {
        let gain = clip.bass_gain;
        let mut previous = 0.0;
        for i in start..end {
            let sample = clip.buffer[i];
            let low = 0.05 * sample + 0.95 * previous;
            previous = low;
            let high = sample - low;
            clip.buffer[i] = ((low * gain) + high).clamp(-1.0, 1.0);
        }
        add_effect_zone(clip, (0.0, 0.67, 0.75));
    } else if effect == "volume" {
        let volume = clip.volume;
        for i in start..end {
            clip.buffer[i] = (clip.buffer[i] * volume).clamp(-1.0, 1.0);
        }
        clip.volume = 1.0;
        add_effect_zone(clip, (0.30, 0.69, 0.30));
    } else if effect == "speed" {
        let speed = clip.speed;
        if (speed - 1.0).abs() > 0.01 {
            let old_frames = (end - start) / channels;
            let new_frames = (old_frames as f64 / speed) as usize;
            let mut new_window = Vec::with_capacity(new_frames * channels);

            for i in 0..new_frames {
                let original_frame = (i as f64 * speed) as usize;
                let original_index = start + original_frame * channels;
                for ch in 0..channels {
                    if original_index + ch < end {
                        new_window.push(clip.buffer[original_index + ch]);
                    } else {
                        new_window.push(0.0);
                    }
                }
            }

            clip.buffer.splice(start..end, new_window);
            clip.speed = 1.0;
            clip.refresh_duration();
            clip.selection = None;
            add_effect_zone(clip, (0.95, 0.26, 0.21));
        }
    }

    clip.amplitudes = compute_amplitudes(&clip.buffer, 900);
}

fn cut_selection(clip: &mut AudioClip) {
    let Some((start, end)) = selection_sample_range(clip) else {
        return;
    };

    let old_len = clip.buffer.len() as f64;
    let removed_len = (end - start) as f64;
    let removed_start_ratio = start as f64 / old_len;
    let removed_end_ratio = end as f64 / old_len;

    clip.buffer.drain(start..end);
    clip.refresh_duration();
    clip.amplitudes = compute_amplitudes(&clip.buffer, 900);
    clip.selection = None;

    let remaining_ratio = 1.0 - (removed_len / old_len);
    if remaining_ratio > 0.0 {
        clip.effect_zones = clip
            .effect_zones
            .iter()
            .filter_map(|(zone_start, zone_end, color)| {
                if *zone_end <= removed_start_ratio {
                    Some((*zone_start / remaining_ratio, *zone_end / remaining_ratio, *color))
                } else if *zone_start >= removed_end_ratio {
                    Some(((zone_start - removed_len / old_len) / remaining_ratio, (zone_end - removed_len / old_len) / remaining_ratio, *color))
                } else {
                    None
                }
            })
            .collect();
    } else {
        clip.effect_zones.clear();
    }
}

fn trim_clip_after_playhead(clip: &mut AudioClip, playhead_secs: f64) {
    if playhead_secs <= clip.start_time_secs || playhead_secs >= clip.end_time_secs() {
        return;
    }

    let relative_secs = playhead_secs - clip.start_time_secs;
    let channels = clip.channels as usize;
    if channels == 0 {
        return;
    }

    let mut cut_sample = (relative_secs * clip.sample_rate as f64 * channels as f64) as usize;
    cut_sample = (cut_sample / channels) * channels;
    cut_sample = cut_sample.min(clip.buffer.len());

    let new_ratio = cut_sample as f64 / clip.buffer.len() as f64;

    clip.buffer.truncate(cut_sample);
    clip.refresh_duration();
    clip.amplitudes = compute_amplitudes(&clip.buffer, 900);
    clip.selection = None;

    clip.effect_zones = clip
        .effect_zones
        .iter()
        .filter_map(|(start, end, color)| {
            if *start >= new_ratio {
                None
            } else {
                let new_start = (*start / new_ratio).clamp(0.0, 1.0);
                let new_end = (end.min(new_ratio) / new_ratio).clamp(0.0, 1.0);
                if new_end > new_start {
                    Some((new_start, new_end, *color))
                } else {
                    None
                }
            }
        })
        .collect();
}

fn load_custom_css() {
    let provider = CssProvider::new();
    let css_data = b"
        .ribbon-panel { background-color: #1e1e1e; border-bottom: 2px solid #3c3c3c; padding: 10px; }
        .track-label { font-size: 14px; font-weight: bold; color: #4fc3f7; }
        .control-label { font-size: 11px; color: #aaaaaa; font-weight: bold; }
        .fx-btn { font-size: 12px; padding: 4px 9px; font-weight: bold; }
    ";
    provider.load_from_data(css_data).expect("CSS Error");
    if let Some(screen) = Screen::get_default() {
        StyleContext::add_provider_for_screen(
            &screen,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[allow(dead_code)]
pub fn run() {
    if gtk::init().is_err() {
        return;
    }
    let window = create_main_window();
    window.show_all();
    gtk::main();
}

pub fn create_main_window() -> Window {
    let settings = Settings::get_default().unwrap();
    settings.set_property_gtk_application_prefer_dark_theme(true);
    load_custom_css();

    let window = Window::new(WindowType::Toplevel);
    window.set_title("MixRust - Timeline DAW");
    window.set_default_size(1300, 760);

    let audio_data = OutputStream::try_default().ok();
    let (_stream, handle) = match audio_data {
        Some((stream, handle)) => (Some(stream), Some(Arc::new(handle))),
        None => (None, None),
    };

    if let Some(stream) = _stream {
        unsafe {
            window.set_data("mixrust_output_stream", stream);
        }
    }

    let timeline = Rc::new(RefCell::new(TimelineState::new()));
    let active_sinks: Rc<RefCell<Vec<Sink>>> = Rc::new(RefCell::new(Vec::new()));

    let main_vbox = GtkBox::new(Orientation::Vertical, 0);
    window.add(&main_vbox);

    let toolbar = Toolbar::new();
    let add_clip_btn = ToolButton::new::<Button>(None, Some("Ajouter Clip"));
    let play_btn = ToolButton::new::<Button>(None, Some("PLAY"));
    let stop_btn = ToolButton::new::<Button>(None, Some("STOP"));
    let export_btn = ToolButton::new::<Button>(None, Some("EXPORT MIX"));
    let zoom_in_btn = ToolButton::new::<Button>(None, Some("ZOOM +"));
    let zoom_out_btn = ToolButton::new::<Button>(None, Some("ZOOM -"));

    toolbar.insert(&add_clip_btn, -1);
    toolbar.insert(&play_btn, -1);
    toolbar.insert(&stop_btn, -1);
    toolbar.insert(&export_btn, -1);
    toolbar.insert(&zoom_in_btn, -1);
    toolbar.insert(&zoom_out_btn, -1);
    main_vbox.pack_start(&toolbar, false, false, 0);

    let ribbon = GtkBox::new(Orientation::Vertical, 6);
    ribbon.get_style_context().add_class("ribbon-panel");

    let info_label = Label::new(Some(
        "Timeline : clique un clip pour le sélectionner. Glisse le haut du clip pour déplacer. Glisse dans la waveform pour sélectionner une zone. Clic droit = annuler sélection.",
    ));
    info_label.get_style_context().add_class("track-label");
    ribbon.pack_start(&info_label, false, false, 3);

    let selected_label = Label::new(Some("Aucun clip sélectionné"));
    selected_label.get_style_context().add_class("control-label");
    ribbon.pack_start(&selected_label, false, false, 3);

    let fx_row = GtkBox::new(Orientation::Horizontal, 10);
    fx_row.set_halign(gtk::Align::Center);

    let scale_vol = Scale::with_range(Orientation::Horizontal, 0.0, 2.0, 0.1);
    scale_vol.set_value(1.0);
    scale_vol.set_size_request(110, -1);

    let scale_speed = Scale::with_range(Orientation::Horizontal, 0.25, 2.0, 0.05);
    scale_speed.set_value(1.0);
    scale_speed.set_size_request(110, -1);

    let scale_bass = Scale::with_range(Orientation::Horizontal, 0.0, 3.0, 0.1);
    scale_bass.set_value(1.0);
    scale_bass.set_size_request(110, -1);

    let btn_apply_vol = Button::with_label("Appliquer Volume");
    let btn_apply_speed = Button::with_label("Appliquer Vitesse");
    let btn_apply_bass = Button::with_label("Appliquer Basses");
    let btn_apply_disto = Button::with_label("Appliquer Distorsion");
    let btn_apply_reverb = Button::with_label("Appliquer Reverb");
    let btn_cut = Button::with_label("Couper sélection");
    let btn_delete_after_cursor = Button::with_label("Suppr après curseur");

    for btn in [
        &btn_apply_vol,
        &btn_apply_speed,
        &btn_apply_bass,
        &btn_apply_disto,
        &btn_apply_reverb,
        &btn_cut,
        &btn_delete_after_cursor,
    ] {
        btn.get_style_context().add_class("fx-btn");
    }

    fx_row.pack_start(&Label::new(Some("Volume")), false, false, 0);
    fx_row.pack_start(&scale_vol, false, false, 0);
    fx_row.pack_start(&btn_apply_vol, false, false, 0);
    fx_row.pack_start(&Label::new(Some("Vitesse")), false, false, 0);
    fx_row.pack_start(&scale_speed, false, false, 0);
    fx_row.pack_start(&btn_apply_speed, false, false, 0);
    fx_row.pack_start(&Label::new(Some("Basses")), false, false, 0);
    fx_row.pack_start(&scale_bass, false, false, 0);
    fx_row.pack_start(&btn_apply_bass, false, false, 0);
    fx_row.pack_start(&btn_apply_disto, false, false, 0);
    fx_row.pack_start(&btn_apply_reverb, false, false, 0);
    fx_row.pack_start(&btn_cut, false, false, 0);
    fx_row.pack_start(&btn_delete_after_cursor, false, false, 0);

    ribbon.pack_start(&fx_row, false, false, 3);
    main_vbox.pack_start(&ribbon, false, false, 0);

    let scroll = ScrolledWindow::new(None::<&Adjustment>, None::<&Adjustment>);
    let drawing_area = DrawingArea::new();
    drawing_area.set_size_request(3000, 700);
    drawing_area.add_events(
        EventMask::BUTTON_PRESS_MASK
            | EventMask::BUTTON_RELEASE_MASK
            | EventMask::POINTER_MOTION_MASK,
    );
    scroll.add(&drawing_area);
    main_vbox.pack_start(&scroll, true, true, 0);

    let update_label = {
        let timeline = Rc::clone(&timeline);
        let selected_label = selected_label.clone();
        move || {
            let state = timeline.borrow();
            if let Some(clip) = state.selected_clip() {
                let selection_text = clip
                    .selection
                    .map(|(a, b)| format!(" | sélection {:.1}% → {:.1}%", a * 100.0, b * 100.0))
                    .unwrap_or_default();
                selected_label.set_text(&format!(
                    "Clip : {} | départ {:.2}s | piste {}{}",
                    clip.name,
                    clip.start_time_secs,
                    clip.lane + 1,
                    selection_text
                ));
            } else {
                selected_label.set_text(&format!("Curseur : {:.2}s", state.playhead_secs));
            }
        }
    };
    let update_label = Rc::new(update_label);

    let window_for_add = window.clone();
    let timeline_for_add = Rc::clone(&timeline);
    let drawing_for_add = drawing_area.clone();
    let update_label_add = Rc::clone(&update_label);
    add_clip_btn.connect_clicked(move |_| {
        let dialog = FileChooserDialog::with_buttons(
            Some("Choisir un fichier audio"),
            Some(&window_for_add),
            FileChooserAction::Open,
            &[("_Annuler", ResponseType::Cancel), ("_Ouvrir", ResponseType::Accept)],
        );

        if dialog.run() == ResponseType::Accept {
            if let Some(path) = dialog.get_filename() {
                let result = {
                    let mut state = timeline_for_add.borrow_mut();
                    state.add_clip_from_file(path)
                };

                match result {
                    Ok(_) => {
                        update_label_add();
                        drawing_for_add.queue_draw();
                    }
                    Err(e) => eprintln!("Erreur ajout clip: {e}"),
                }
            }
        }
        dialog.close();
    });

    let connect_scale = |scale: &Scale, timeline: Rc<RefCell<TimelineState>>, field: &'static str| {
        scale.connect_value_changed(move |scale| {
            if let Some(clip) = timeline.borrow_mut().selected_clip_mut() {
                match field {
                    "volume" => clip.volume = scale.get_value() as f32,
                    "speed" => clip.speed = scale.get_value(),
                    "bass" => clip.bass_gain = scale.get_value() as f32,
                    _ => {}
                }
            }
        });
    };
    connect_scale(&scale_vol, Rc::clone(&timeline), "volume");
    connect_scale(&scale_speed, Rc::clone(&timeline), "speed");
    connect_scale(&scale_bass, Rc::clone(&timeline), "bass");

    let connect_fx = |btn: &Button,
                      effect: &'static str,
                      timeline: Rc<RefCell<TimelineState>>,
                      drawing: DrawingArea,
                      update_label: Rc<dyn Fn()>| {
        btn.connect_clicked(move |_| {
            {
                let mut state = timeline.borrow_mut();
                if let Some(clip) = state.selected_clip_mut() {
                    apply_effect_to_clip(clip, effect);
                }
            }
            update_label();
            drawing.queue_draw();
        });
    };

    connect_fx(&btn_apply_vol, "volume", Rc::clone(&timeline), drawing_area.clone(), update_label.clone());
    connect_fx(&btn_apply_speed, "speed", Rc::clone(&timeline), drawing_area.clone(), update_label.clone());
    connect_fx(&btn_apply_bass, "bass", Rc::clone(&timeline), drawing_area.clone(), update_label.clone());
    connect_fx(&btn_apply_disto, "disto", Rc::clone(&timeline), drawing_area.clone(), update_label.clone());
    connect_fx(&btn_apply_reverb, "reverb", Rc::clone(&timeline), drawing_area.clone(), update_label.clone());

    let timeline_cut = Rc::clone(&timeline);
    let drawing_cut = drawing_area.clone();
    let update_label_cut = Rc::clone(&update_label);
    btn_cut.connect_clicked(move |_| {
        {
            let mut state = timeline_cut.borrow_mut();
            if let Some(clip) = state.selected_clip_mut() {
                cut_selection(clip);
            }
        }
        update_label_cut();
        drawing_cut.queue_draw();
    });

    let timeline_delete = Rc::clone(&timeline);
    let drawing_delete = drawing_area.clone();
    let update_label_delete = Rc::clone(&update_label);
    btn_delete_after_cursor.connect_clicked(move |_| {
        let playhead = {
            let state = timeline_delete.borrow();
            state.playhead_secs
        };

        {
            let mut state = timeline_delete.borrow_mut();
            if let Some(clip) = state.selected_clip_mut() {
                trim_clip_after_playhead(clip, playhead);
            }
        }

        update_label_delete();
        drawing_delete.queue_draw();
    });

    let timeline_for_zoom_in = Rc::clone(&timeline);
    let drawing_for_zoom_in = drawing_area.clone();
    zoom_in_btn.connect_clicked(move |_| {
        let mut state = timeline_for_zoom_in.borrow_mut();
        state.pixels_per_second = (state.pixels_per_second * 1.25).min(500.0);
        drawing_for_zoom_in.set_size_request(
            (HEADER_WIDTH + state.timeline_duration_secs() * state.pixels_per_second + 300.0) as i32,
            (RULER_HEIGHT + state.lanes as f64 * TRACK_HEIGHT + 80.0) as i32,
        );
        drawing_for_zoom_in.queue_draw();
    });

    let timeline_for_zoom_out = Rc::clone(&timeline);
    let drawing_for_zoom_out = drawing_area.clone();
    zoom_out_btn.connect_clicked(move |_| {
        let mut state = timeline_for_zoom_out.borrow_mut();
        state.pixels_per_second = (state.pixels_per_second / 1.25).max(20.0);
        drawing_for_zoom_out.set_size_request(
            (HEADER_WIDTH + state.timeline_duration_secs() * state.pixels_per_second + 300.0) as i32,
            (RULER_HEIGHT + state.lanes as f64 * TRACK_HEIGHT + 80.0) as i32,
        );
        drawing_for_zoom_out.queue_draw();
    });

    let timeline_for_press = Rc::clone(&timeline);
    let update_label_press = Rc::clone(&update_label);
    drawing_area.connect_button_press_event(move |drawing_area, event| {
        let (x, y) = event.get_position();
        let button = event.get_button();

        {
            let mut state = timeline_for_press.borrow_mut();

            if button == 3 {
                if let Some(clip) = state.selected_clip_mut() {
                    clip.selection = None;
                }
                state.mouse_mode = MouseMode::None;
                state.drag_clip_id = None;
            } else if let Some(clip_id) = state.hit_test_clip(x, y) {
                state.selected_clip_id = Some(clip_id);

                let clip_info = state
                    .clips
                    .iter()
                    .find(|clip| clip.id == clip_id)
                    .map(|clip| (clip.start_time_secs, clip.duration_secs, clip.lane));

                if let Some((clip_start, clip_duration, lane)) = clip_info {
                    let mouse_time = (x - HEADER_WIDTH) / state.pixels_per_second;
                    let ratio = ((mouse_time - clip_start) / clip_duration).clamp(0.0, 1.0);
                    let lane_y = RULER_HEIGHT + lane as f64 * TRACK_HEIGHT;

                    if y < lane_y + CLIP_MARGIN_Y + 22.0 {
                        state.mouse_mode = MouseMode::MoveClip;
                        state.drag_clip_id = Some(clip_id);
                        state.drag_offset_secs = mouse_time - clip_start;
                    } else {
                        state.mouse_mode = MouseMode::SelectInClip;
                        state.drag_clip_id = Some(clip_id);
                        state.selection_start_ratio = ratio;

                        if let Some(clip) = state.selected_clip_mut() {
                            clip.selection = Some((ratio, ratio));
                        }
                    }
                }
            } else if x >= HEADER_WIDTH && y >= RULER_HEIGHT {
                state.selected_clip_id = None;
                state.mouse_mode = MouseMode::None;
                state.drag_clip_id = None;
                state.playhead_secs = ((x - HEADER_WIDTH) / state.pixels_per_second).max(0.0);
            }
        }

        update_label_press();
        drawing_area.queue_draw();
        Inhibit(false)
    });

    let timeline_for_motion = Rc::clone(&timeline);
    let update_label_motion = Rc::clone(&update_label);
    drawing_area.connect_motion_notify_event(move |drawing_area, event| {
        let (x, y) = event.get_position();

        {
            let mut state = timeline_for_motion.borrow_mut();

            if let Some(clip_id) = state.drag_clip_id {
                match state.mouse_mode {
                    MouseMode::MoveClip => {
                        state.set_clip_position_from_mouse(clip_id, x, y);
                        drawing_area.set_size_request(
                            (HEADER_WIDTH + state.timeline_duration_secs() * state.pixels_per_second + 300.0) as i32,
                            (RULER_HEIGHT + state.lanes as f64 * TRACK_HEIGHT + 80.0) as i32,
                        );
                    }
                    MouseMode::SelectInClip => {
                        if let Some(ratio) = state.clip_ratio_at_mouse(clip_id, x) {
                            let start = state.selection_start_ratio;
                            if let Some(clip) = state.clips.iter_mut().find(|clip| clip.id == clip_id) {
                                clip.selection = Some((start.min(ratio), start.max(ratio)));
                            }
                        }
                    }
                    MouseMode::None => {}
                }
            }
        }

        update_label_motion();
        drawing_area.queue_draw();
        Inhibit(false)
    });

    let timeline_for_release = Rc::clone(&timeline);
    drawing_area.connect_button_release_event(move |drawing_area, _event| {
        let mut state = timeline_for_release.borrow_mut();
        state.mouse_mode = MouseMode::None;
        state.drag_clip_id = None;
        drawing_area.queue_draw();
        Inhibit(false)
    });

    let timeline_for_draw = Rc::clone(&timeline);
    drawing_area.connect_draw(move |drawing_area, cr| {
        let width = drawing_area.get_allocated_width() as f64;
        let height = drawing_area.get_allocated_height() as f64;
        let state = timeline_for_draw.borrow();

        cr.set_source_rgb(0.10, 0.10, 0.11);
        cr.rectangle(0.0, 0.0, width, height);
        cr.fill();

        draw_ruler(cr, &state, width);
        draw_lanes(cr, &state, width);
        draw_clips(cr, &state);
        draw_playhead(cr, &state, height);
        Inhibit(false)
    });

    let timeline_for_play = Rc::clone(&timeline);
    let sinks_for_play = Rc::clone(&active_sinks);
    let handle_for_play = handle.clone();
    let drawing_for_play = drawing_area.clone();
    play_btn.connect_clicked(move |_| {
        stop_all_sinks(&sinks_for_play);

        {
            let mut state = timeline_for_play.borrow_mut();
            state.is_playing = true;
            state.last_tick = Some(Instant::now());
        }

        if let Some(handle) = handle_for_play.as_ref() {
            let state = timeline_for_play.borrow();
            let playhead = state.playhead_secs;

            for clip in state.clips.iter() {
                if clip.end_time_secs() <= playhead {
                    continue;
                }

                let clip_start_delay = (clip.start_time_secs - playhead).max(0.0);
                let start_inside_clip = (playhead - clip.start_time_secs).max(0.0);
                let audio_offset = clip.offset_in_audio_secs + start_inside_clip;

                if let Ok(sink) = Sink::try_new(handle) {
                    let source = make_clip_source(clip, audio_offset, clip_start_delay);
                    sink.set_volume(1.0);
                    sink.append(source);
                    sink.play();
                    sinks_for_play.borrow_mut().push(sink);
                }
            }
        }

        drawing_for_play.queue_draw();
    });

    let timeline_for_stop = Rc::clone(&timeline);
    let sinks_for_stop = Rc::clone(&active_sinks);
    let drawing_for_stop = drawing_area.clone();
    stop_btn.connect_clicked(move |_| {
        stop_all_sinks(&sinks_for_stop);
        let mut state = timeline_for_stop.borrow_mut();
        state.is_playing = false;
        state.last_tick = None;
        drawing_for_stop.queue_draw();
    });

    let timeline_for_export = Rc::clone(&timeline);
    export_btn.connect_clicked(move |_| {
        let folder = PathBuf::from("enregistrements");
        if !folder.exists() {
            let _ = fs::create_dir_all(&folder);
        }
        let output_path = folder.join("mix_timeline.wav");

        match export_timeline_mix(&timeline_for_export.borrow(), &output_path) {
            Ok(_) => println!("Mix timeline exporté : {}", output_path.display()),
            Err(e) => eprintln!("Erreur export timeline : {e}"),
        }
    });

    let timeline_for_timer = Rc::clone(&timeline);
    let drawing_for_timer = drawing_area.clone();
    glib::timeout_add_local(30, move || {
        let mut state = timeline_for_timer.borrow_mut();
        if state.is_playing {
            let now = Instant::now();
            if let Some(last) = state.last_tick {
                state.playhead_secs += now.duration_since(last).as_secs_f64();
            }
            state.last_tick = Some(now);

            if state.playhead_secs >= state.timeline_duration_secs() {
                state.is_playing = false;
                state.last_tick = None;
            }
            drawing_for_timer.queue_draw();
        }
        glib::Continue(true)
    });

    window.connect_delete_event(|_, _| {
        gtk::main_quit();
        Inhibit(false)
    });

    window
}

fn draw_ruler(cr: &cairo::Context, state: &TimelineState, width: f64) {
    cr.set_source_rgb(0.16, 0.16, 0.18);
    cr.rectangle(0.0, 0.0, width, RULER_HEIGHT);
    cr.fill();

    cr.set_source_rgb(0.28, 0.28, 0.30);
    cr.rectangle(0.0, 0.0, HEADER_WIDTH, RULER_HEIGHT);
    cr.fill();

    let duration = state.timeline_duration_secs();
    let step = if state.pixels_per_second < 35.0 { 5.0 } else { 1.0 };
    let mut time = 0.0;

    while time <= duration + 10.0 {
        let x = HEADER_WIDTH + time * state.pixels_per_second;
        let major = (time as i32) % 5 == 0;

        cr.set_source_rgb(0.42, 0.42, 0.44);
        cr.set_line_width(if major { 1.5 } else { 1.0 });
        cr.move_to(x, if major { 0.0 } else { RULER_HEIGHT * 0.45 });
        cr.line_to(x, RULER_HEIGHT);
        cr.stroke();

        if major {
            cr.set_source_rgb(0.78, 0.78, 0.80);
            cr.move_to(x + 4.0, 18.0);
            let _ = cr.show_text(&format!("{}s", time as i32));
        }

        time += step;
    }
}

fn draw_lanes(cr: &cairo::Context, state: &TimelineState, width: f64) {
    for lane in 0..state.lanes {
        let y = RULER_HEIGHT + lane as f64 * TRACK_HEIGHT;

        if lane % 2 == 0 {
            cr.set_source_rgb(0.13, 0.13, 0.15);
        } else {
            cr.set_source_rgb(0.11, 0.11, 0.13);
        }
        cr.rectangle(0.0, y, width, TRACK_HEIGHT);
        cr.fill();

        cr.set_source_rgb(0.18, 0.18, 0.20);
        cr.rectangle(0.0, y, HEADER_WIDTH, TRACK_HEIGHT);
        cr.fill();

        cr.set_source_rgb(0.30, 0.30, 0.32);
        cr.set_line_width(1.0);
        cr.move_to(0.0, y + TRACK_HEIGHT);
        cr.line_to(width, y + TRACK_HEIGHT);
        cr.stroke();

        cr.set_source_rgb(0.65, 0.65, 0.68);
        cr.move_to(18.0, y + 30.0);
        let _ = cr.show_text(&format!("Piste {}", lane + 1));
    }

    let duration = state.timeline_duration_secs();
    let mut time = 0.0;
    while time <= duration + 10.0 {
        let x = HEADER_WIDTH + time * state.pixels_per_second;
        cr.set_source_rgba(
            1.0,
            1.0,
            1.0,
            if (time as i32) % 5 == 0 { 0.12 } else { 0.045 },
        );
        cr.move_to(x, RULER_HEIGHT);
        cr.line_to(x, RULER_HEIGHT + state.lanes as f64 * TRACK_HEIGHT);
        cr.stroke();
        time += 1.0;
    }
}

fn draw_clips(cr: &cairo::Context, state: &TimelineState) {
    for clip in &state.clips {
        let x = HEADER_WIDTH + clip.start_time_secs * state.pixels_per_second;
        let y = RULER_HEIGHT + clip.lane as f64 * TRACK_HEIGHT + CLIP_MARGIN_Y;
        let width = (clip.duration_secs * state.pixels_per_second).max(8.0);
        let height = TRACK_HEIGHT - CLIP_MARGIN_Y * 2.0;
        let selected = state.selected_clip_id == Some(clip.id);

        cr.set_source_rgb(clip.color.0 * 0.55, clip.color.1 * 0.55, clip.color.2 * 0.55);
        cr.rectangle(x, y, width, height);
        cr.fill();

        for (start, end, color) in &clip.effect_zones {
            cr.set_source_rgba(color.0, color.1, color.2, 0.65);
            cr.rectangle(x + start * width, y, (end - start) * width, height);
            cr.fill();
        }

        if let Some((a, b)) = clip.selection {
            cr.set_source_rgba(1.0, 0.85, 0.0, 0.35);
            cr.rectangle(x + a * width, y, (b - a) * width, height);
            cr.fill();
        }

        cr.set_source_rgb(clip.color.0, clip.color.1, clip.color.2);
        cr.set_line_width(if selected { 3.0 } else { 1.5 });
        cr.rectangle(x, y, width, height);
        cr.stroke();

        cr.set_source_rgb(0.95, 0.95, 0.95);
        cr.move_to(x + 8.0, y + 17.0);
        let _ = cr.show_text(&clip.name);

        draw_waveform_in_clip(cr, clip, x, y + 22.0, width, height - 28.0);
    }
}

fn draw_waveform_in_clip(cr: &cairo::Context, clip: &AudioClip, x: f64, y: f64, width: f64, height: f64) {
    let mid_y = y + height / 2.0;
    let amps = &clip.amplitudes;
    if amps.is_empty() {
        return;
    }

    let bars = width.min(amps.len() as f64) as usize;
    if bars == 0 {
        return;
    }

    cr.set_source_rgba(0.02, 0.02, 0.03, 0.35);
    cr.rectangle(x + 4.0, y, (width - 8.0).max(1.0), height);
    cr.fill();

    cr.set_source_rgb(0.90, 0.95, 1.0);
    cr.set_line_width(1.0);

    for i in 0..bars {
        let amp_idx = i * amps.len() / bars;
        let amp = amps[amp_idx] as f64;
        let bar_x = x + 5.0 + i as f64 * ((width - 10.0).max(1.0) / bars as f64);
        let bar_height = (amp * height * 0.48).max(1.0);
        cr.move_to(bar_x, mid_y - bar_height);
        cr.line_to(bar_x, mid_y + bar_height);
        cr.stroke();
    }
}

fn draw_playhead(cr: &cairo::Context, state: &TimelineState, height: f64) {
    let x = HEADER_WIDTH + state.playhead_secs * state.pixels_per_second;
    cr.set_source_rgb(1.0, 0.12, 0.12);
    cr.set_line_width(2.0);
    cr.move_to(x, 0.0);
    cr.line_to(x, height);
    cr.stroke();
}

fn stop_all_sinks(sinks: &Rc<RefCell<Vec<Sink>>>) {
    for sink in sinks.borrow_mut().drain(..) {
        sink.stop();
    }
}

fn make_clip_source(
    clip: &AudioClip,
    audio_offset_secs: f64,
    start_delay_secs: f64,
) -> Box<dyn Source<Item = f32> + Send> {
    let channels = clip.channels as usize;
    let sample_rate = clip.sample_rate;

    let mut start_sample = (audio_offset_secs * sample_rate as f64 * channels as f64) as usize;
    start_sample = (start_sample / channels) * channels;
    start_sample = start_sample.min(clip.buffer.len());

    let clip_remaining_secs =
        (clip.duration_secs - (audio_offset_secs - clip.offset_in_audio_secs)).max(0.0);
    let mut sample_count = (clip_remaining_secs * sample_rate as f64 * channels as f64) as usize;
    sample_count = (sample_count / channels) * channels;

    let end_sample = (start_sample + sample_count).min(clip.buffer.len());
    let data = clip.buffer[start_sample..end_sample].to_vec();

    let source = SamplesBuffer::new(clip.channels, sample_rate, data)
        .delay(std::time::Duration::from_secs_f64(start_delay_secs));
    Box::new(source)
}

fn export_timeline_mix(
    state: &TimelineState,
    output_path: &PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    if state.clips.is_empty() {
        return Err("Aucun clip à exporter".into());
    }

    let ref_channels = state.clips[0].channels;
    let ref_sample_rate = state.clips[0].sample_rate;
    let channels = ref_channels as usize;
    let total_duration = state.timeline_duration_secs();
    let total_samples = (total_duration * ref_sample_rate as f64 * channels as f64) as usize;
    let mut mix = vec![0.0f32; total_samples];

    for clip in &state.clips {
        if clip.channels != ref_channels || clip.sample_rate != ref_sample_rate {
            eprintln!("Clip ignoré car format différent: {}", clip.name);
            continue;
        }

        let mut dst_start = (clip.start_time_secs * ref_sample_rate as f64 * channels as f64) as usize;
        dst_start = (dst_start / channels) * channels;

        let mut src_start = (clip.offset_in_audio_secs * ref_sample_rate as f64 * channels as f64) as usize;
        src_start = (src_start / channels) * channels;

        let mut sample_count = (clip.duration_secs * ref_sample_rate as f64 * channels as f64) as usize;
        sample_count = (sample_count / channels) * channels;

        for i in 0..sample_count {
            let src = src_start + i;
            let dst = dst_start + i;
            if src < clip.buffer.len() && dst < mix.len() {
                mix[dst] += clip.buffer[src];
            }
        }
    }

    for sample in &mut mix {
        *sample = sample.clamp(-1.0, 1.0);
    }

    let spec = hound::WavSpec {
        channels: ref_channels,
        sample_rate: ref_sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = hound::WavWriter::create(output_path, spec)?;
    for sample in mix {
        writer.write_sample((sample * i16::MAX as f32) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}
