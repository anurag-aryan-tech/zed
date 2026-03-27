use anyhow::Context as _;
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Bounds, Context, Element, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, ParentElement, Pixels, Render,
    SharedString, Style, Styled, Task, WeakEntity, Window, actions, div,
    point, px, relative, size,
};
use image::{ImageBuffer, Rgba};
use parking_lot::Mutex;
use project::{Project, ProjectEntryId, ProjectPath};
use smallvec::SmallVec;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use ui::prelude::*;
use workspace::{
    ItemId, Pane, WorkspaceId, delete_unloaded_items,
    invalid_item_view::InvalidItemView,
    item::{Item, ItemHandle, ProjectItem, SerializableItem, TabContentParams},
};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_app::prelude::AppSinkExt;

// ---------------------------------------------------------------------------
// Video file extensions we handle
// ---------------------------------------------------------------------------
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "qt", "webm", "mkv", "avi", "wmv",
    "flv", "ogv", "ogg", "ts", "mts", "m2ts", "3gp", "3g2",
];

fn is_video_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

fn format_time(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

// ---------------------------------------------------------------------------
// Keyboard actions
// ---------------------------------------------------------------------------
actions!(video_viewer, [TogglePlayPause, SeekForward, SeekBackward, ToggleMute]);

// ---------------------------------------------------------------------------
// VideoItem — the project data model (like ImageItem for images)
// Zed uses this to decide "which crate handles this file?"
// ---------------------------------------------------------------------------
pub struct VideoItem {
    pub project_path: ProjectPath,
    pub abs_path: PathBuf,
}

impl project::ProjectItem for VideoItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<Entity<Self>>>> {
        if !is_video_path(&path.path) {
            return None;
        }
        let path = path.clone();
        let abs_path = project
            .read(cx)
            .worktree_for_id(path.worktree_id, cx)
            .map(|wt| wt.read(cx).abs_path().join(&path.path))
            .unwrap_or_else(|| PathBuf::from(path.path.as_ref()));
        Some(cx.spawn(async move |cx| {
            cx.update(|_, cx| {
                anyhow::Ok(cx.new(|_| VideoItem { project_path: path, abs_path }))
            })?
        }))
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        None
    }

    fn project_path(&self, _: &App) -> ProjectPath {
        self.project_path.clone()
    }
}

// ---------------------------------------------------------------------------
// PlaybackState — simple enum tracking if video is playing or paused
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, PartialEq)]
enum PlaybackState {
    Playing,
    Paused,
}

// ---------------------------------------------------------------------------
// RgbaFrame — holds one decoded video frame as raw RGBA pixels
// ---------------------------------------------------------------------------
struct RgbaFrame {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

// ---------------------------------------------------------------------------
// VideoViewEvent — events we emit to tell Zed our state changed
// ---------------------------------------------------------------------------
pub enum VideoViewEvent {
    TitleChanged,
}

// ---------------------------------------------------------------------------
// VideoView — the main struct, holds all state for the video player
// ---------------------------------------------------------------------------
pub struct VideoView {
    // File info
    path: PathBuf,
    project: Entity<Project>,
    focus_handle: FocusHandle,

    // GStreamer pipeline — the video decoding engine
    pipeline: Option<gst::Pipeline>,

    // Current decoded frame — shared between GStreamer thread and UI thread
    // Arc = shared ownership, Mutex = safe access from multiple threads
    current_frame: Arc<Mutex<Option<RgbaFrame>>>,

    // AtomicBool = thread-safe true/false flag
    // Set to true by GStreamer thread when a new frame arrives
    // Set to false by UI thread after it reads the frame
    frame_ready: Arc<AtomicBool>,

    // Playback state
    playback: PlaybackState,
    position: Duration,
    duration: Duration,
    volume: f64,
    prev_volume: f64,   // remembered volume before muting
    speed: f64,

    // UI state
    loading: bool,
    error: Option<String>,
    controls_visible: bool,
    last_interaction: Instant,

    // For serialization (saving/restoring position)
    saved_position: Option<Duration>,
}

impl VideoView {
    pub fn new(
        item: Entity<VideoItem>,
        project: Entity<Project>,
        saved_position: Option<Duration>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = item.read(cx).abs_path.clone();

        // Shared state between GStreamer worker thread and UI thread
        let current_frame: Arc<Mutex<Option<RgbaFrame>>> = Arc::new(Mutex::new(None));
        let frame_ready = Arc::new(AtomicBool::new(false));

        let mut view = Self {
            path: path.clone(),
            project,
            focus_handle: cx.focus_handle(),
            pipeline: None,
            current_frame,
            frame_ready,
            playback: PlaybackState::Playing,
            position: Duration::ZERO,
            duration: Duration::ZERO,
            volume: 0.0,   // muted by default
            prev_volume: 1.0,
            speed: 1.0,
            loading: true,
            error: None,
            controls_visible: true,
            last_interaction: Instant::now(),
            saved_position,
        };

        // Start the GStreamer pipeline
        match view.start_pipeline(cx) {
            Ok(_) => {}
            Err(e) => {
                view.loading = false;
                view.error = Some(format!("Failed to start video: {}", e));
            }
        }

        // Schedule periodic UI refresh so position counter updates
        // This runs every 250ms to update the seek bar position
        cx.spawn(async move |handle, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if handle.update(cx, |this, cx| {
                    if let Some(pipeline) = &this.pipeline {
                        if let Some(pos) = pipeline.query_position::<gst::ClockTime>() {
                            this.position = Duration::from_nanos(pos.nseconds());
                        }
                        if let Some(dur) = pipeline.query_duration::<gst::ClockTime>() {
                            if dur.nseconds() > 0 {
                                this.duration = Duration::from_nanos(dur.nseconds());
                            }
                        }
                    }
                    // Hide controls after 3 seconds of no interaction
                    if this.controls_visible
                        && this.last_interaction.elapsed() > Duration::from_secs(3)
                        && this.playback == PlaybackState::Playing
                    {
                        this.controls_visible = false;
                    }
                    cx.notify();
                }).is_err() {
                    break;
                }
            }
        }).detach();

        view
    }

    fn start_pipeline(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        // Initialize GStreamer (safe to call multiple times)
        gst::init().context("Failed to initialize GStreamer")?;

        let path_str = self.path.to_string_lossy();

        // Build the URI — GStreamer needs file:/// format on all platforms
        let uri = if path_str.starts_with('/') {
            format!("file://{}", path_str)
        } else {
            // Windows path like C:\videos\movie.mp4
            format!("file:///{}", path_str.replace('\\', "/"))
        };

        // The GStreamer pipeline string:
        // filesrc reads the file → decodebin figures out the format →
        // videoconvert converts to RGBA → appsink hands frames to us
        // The audio branch plays through speakers
        let pipeline_str = format!(
            "playbin uri=\"{}\" video-sink=\"videoconvert ! video/x-raw,format=BGRA ! appsink name=vsink drop=true max-buffers=3 enable-last-sample=false\"",
            uri
        );

        let pipeline = gst::parse::launch(&pipeline_str)
            .context("Failed to create GStreamer pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("Pipeline is not a Pipeline element"))?;

        // Get the appsink element so we can receive frames
        let video_sink_element: gst::Element = pipeline.property("video-sink");
        let bin = video_sink_element
            .downcast::<gst::Bin>()
            .map_err(|_| anyhow::anyhow!("video-sink is not a bin"))?;
        let appsink = bin
            .by_name("vsink")
            .context("Could not find appsink named vsink")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("vsink is not an AppSink"))?;

        // Set initial volume to 0.0 (muted by default)
        pipeline.set_property("volume", 0.0f64);

        // Clone shared state for the GStreamer callback thread
        let current_frame = Arc::clone(&self.current_frame);
        let frame_ready = Arc::clone(&self.frame_ready);

        // This callback runs every time GStreamer decodes a new video frame
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    // Pull the sample (frame) from GStreamer
                    let sample = match sink.pull_sample() {
                        Ok(s) => s,
                        Err(_) => return Err(gst::FlowError::Error),
                    };

                    let buffer = match sample.buffer() {
                        Some(b) => b,
                        None => return Err(gst::FlowError::Error),
                    };

                    let caps = match sample.caps() {
                        Some(c) => c,
                        None => return Err(gst::FlowError::Error),
                    };

                    let s = match caps.structure(0) {
                        Some(s) => s,
                        None => return Err(gst::FlowError::Error),
                    };

                    let width: i32 = s.get("width").unwrap_or(0);
                    let height: i32 = s.get("height").unwrap_or(0);

                    if width <= 0 || height <= 0 {
                        return Err(gst::FlowError::Error);
                    }

                    let map = match buffer.map_readable() {
                        Ok(m) => m,
                        Err(_) => return Err(gst::FlowError::Error),
                    };

                    // Copy pixels into our RgbaFrame struct
                    let frame = RgbaFrame {
                        data: map.as_slice().to_vec(),
                        width: width as u32,
                        height: height as u32,
                    };

                    // Store it safely (the Mutex ensures only one thread writes at a time)
                    *current_frame.lock() = Some(frame);

                    // Signal the UI thread: "new frame is ready, please redraw"
                    frame_ready.store(true, Ordering::SeqCst);

                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // Start playing
        pipeline
            .set_state(gst::State::Playing)
            .context("Failed to start pipeline")?;

        // Listen for End-of-Stream so we can loop
        let pipeline_weak = pipeline.downgrade();
        let entity = cx.weak_entity();
        let bus = pipeline.bus().context("No bus")?;

        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;

                let Some(pipeline) = pipeline_weak.upgrade() else { break };

                // Check for messages on the GStreamer bus
                while let Some(msg) = bus.pop() {
                    match msg.view() {
                        gst::MessageView::Eos(_) => {
                            // End of stream — seek back to beginning to loop
                            let _ = pipeline.seek_simple(
                                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                gst::ClockTime::from_seconds(0),
                            );
                            let _ = pipeline.set_state(gst::State::Playing);
                        }
                        gst::MessageView::Error(err) => {
                            let msg = err.error().to_string();
                            let _ = entity.update(cx, |this, cx| {
                                this.loading = false;
                                this.error = Some(format!("Playback error: {}", msg));
                                cx.notify();
                            });
                        }
                        gst::MessageView::AsyncDone(_) => {
                            // Pipeline finished seeking/starting — no longer loading
                            let _ = entity.update(cx, |this, cx| {
                                this.loading = false;
                                cx.notify();
                            });
                        }
                        _ => {}
                    }
                }
            }
        }).detach();

        self.pipeline = Some(pipeline);
        Ok(())
    }

    fn toggle_play_pause(&mut self, _: &TogglePlayPause, _window: &mut Window, cx: &mut Context<Self>) {
        self.last_interaction = Instant::now();
        self.controls_visible = true;
        if self.playback == PlaybackState::Playing {
            self.playback = PlaybackState::Paused;
            if let Some(p) = &self.pipeline {
                let _ = p.set_state(gst::State::Paused);
            }
        } else {
            self.playback = PlaybackState::Playing;
            if let Some(p) = &self.pipeline {
                let _ = p.set_state(gst::State::Playing);
            }
        }
        cx.notify();
    }

    fn seek_forward(&mut self, _: &SeekForward, _window: &mut Window, cx: &mut Context<Self>) {
        self.last_interaction = Instant::now();
        self.controls_visible = true;
        let target = self.position.saturating_add(Duration::from_secs(5));
        let target = target.min(self.duration);
        if let Some(p) = &self.pipeline {
            let _ = p.seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_nseconds(target.as_nanos() as u64),
            );
        }
        cx.notify();
    }

    fn seek_backward(&mut self, _: &SeekBackward, _window: &mut Window, cx: &mut Context<Self>) {
        self.last_interaction = Instant::now();
        self.controls_visible = true;
        let target = self.position.saturating_sub(Duration::from_secs(5));
        if let Some(p) = &self.pipeline {
            let _ = p.seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_nseconds(target.as_nanos() as u64),
            );
        }
        cx.notify();
    }

    fn toggle_mute(&mut self, _: &ToggleMute, _window: &mut Window, cx: &mut Context<Self>) {
        self.last_interaction = Instant::now();
        self.controls_visible = true;
        if self.volume == 0.0 {
            self.volume = self.prev_volume.max(0.1);
        } else {
            self.prev_volume = self.volume;
            self.volume = 0.0;
        }
        if let Some(p) = &self.pipeline {
            p.set_property("volume", self.volume);
        }
        cx.notify();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.loading = true;
        self.pipeline = None;
        match self.start_pipeline(cx) {
            Ok(_) => {}
            Err(e) => {
                self.loading = false;
                self.error = Some(format!("Failed to start video: {}", e));
            }
        }
        cx.notify();
    }
}

impl Drop for VideoView {
    fn drop(&mut self) {
        // Clean up GStreamer pipeline when view is closed
        if let Some(pipeline) = self.pipeline.take() {
            let _ = pipeline.set_state(gst::State::Null);
        }
    }
}

// ---------------------------------------------------------------------------
// PART 3 — Renderer and Controls UI
// ---------------------------------------------------------------------------

struct VideoElement {
    view: Entity<VideoView>,
}

impl VideoElement {
    fn new(view: Entity<VideoView>) -> Self {
        Self { view }
    }

    /// Compute letterboxed bounds — fits video inside container keeping aspect ratio
    fn fitted_bounds(
        container: Bounds<Pixels>,
        frame_w: u32,
        frame_h: u32,
    ) -> Bounds<Pixels> {
        let cw: f32 = container.size.width.into();
        let ch: f32 = container.size.height.into();
        let fw = frame_w as f32;
        let fh = frame_h as f32;
        let scale = if fw > 0.0 && fh > 0.0 {
            (cw / fw).min(ch / fh)
        } else {
            1.0
        };
        let dw = fw * scale;
        let dh = fh * scale;
        let ox = (cw - dw) * 0.5;
        let oy = (ch - dh) * 0.5;
        Bounds::new(
            point(container.origin.x + px(ox), container.origin.y + px(oy)),
            size(px(dw), px(dh)),
        )
    }
}

impl IntoElement for VideoElement {
    type Element = Self;
    fn into_element(self) -> Self::Element { self }
}

impl Element for VideoElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> { None }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> { None }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout = window.request_layout(
            Style {
                size: size(relative(1.).into(), relative(1.).into()),
                ..Default::default()
            },
            [],
            cx,
        );
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // Request animation frame if playing or new frame ready
        let view = self.view.read(cx);
        let is_playing = view.playback == PlaybackState::Playing;
        let has_frame = view.frame_ready.load(Ordering::SeqCst);
        if is_playing || has_frame {
            window.request_animation_frame();
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Mark frame as consumed
        self.view.read(cx).frame_ready.store(false, Ordering::SeqCst);

        // Get the current frame pixels
        let frame_data = {
            let view = self.view.read(cx);
            let guard = view.current_frame.lock();
            guard.as_ref().map(|f| (f.data.clone(), f.width, f.height))
        };

        let Some((data, width, height)) = frame_data else { return };

        // Convert raw BGRA bytes into an image GPUI can display
        let Some(image_buffer) = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, data) else { return };

        let frames: SmallVec<[image::Frame; 1]> =
            SmallVec::from_elem(image::Frame::new(image_buffer), 1);
        let render_image = Arc::new(gpui::RenderImage::new(frames));

        // Remember previous image so we can drop it after painting
        // This prevents GPU memory from growing every frame
        let prev_image: Entity<Option<Arc<gpui::RenderImage>>> =
            window.use_state(cx, |_, _| None);

        let prev = prev_image.update(cx, |slot, _| slot.replace(render_image.clone()));

        // Paint video frame letterboxed inside container
        let dest = Self::fitted_bounds(bounds, width, height);
        window
            .paint_image(dest, gpui::Corners::default(), render_image, 0, false)
            .ok();

        // Drop previous frame texture from GPU atlas to prevent memory leak
        if let Some(old) = prev {
            cx.drop_image(old, Some(window));
        }
    }
}

// ---------------------------------------------------------------------------
// Render trait — what Zed calls to draw our UI every frame
// ---------------------------------------------------------------------------
impl Render for VideoView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();

        div()
            .track_focus(&focus)
            .key_context("VideoViewer")
            .on_action(cx.listener(Self::toggle_play_pause))
            .on_action(cx.listener(Self::seek_forward))
            .on_action(cx.listener(Self::seek_backward))
            .on_action(cx.listener(Self::toggle_mute))
            .size_full()
            .relative()
            .bg(gpui::black())
            .on_mouse_move(cx.listener(|this, _, _, cx| {
                this.controls_visible = true;
                this.last_interaction = Instant::now();
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.last_interaction = Instant::now();
                    this.controls_visible = true;
                    this.toggle_play_pause(&TogglePlayPause, window, cx);
                }),
            )
            .child(self.render_content(cx))
            .child(self.render_controls(cx))
    }
}

impl VideoView {
    /// Renders the main content area — loading spinner, error, or video frame
    fn render_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.loading {
            // Loading spinner centered in pane
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(cx.theme().colors().text_muted)
                        .child("Loading video...")
                )
                .into_any_element()
        } else if let Some(err) = &self.error {
            // Error message with retry button
            let err = err.clone();
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .child(
                    div()
                        .text_color(cx.theme().status().error)
                        .child(err)
                )
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .bg(cx.theme().colors().element_background)
                        .rounded_md()
                        .cursor_pointer()
                        .text_color(cx.theme().colors().text)
                        .child("Retry")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                                this.retry(cx);
                            }),
                        )
                )
                .into_any_element()
        } else {
            // Video frame
            div()
                .size_full()
                .child(VideoElement::new(cx.entity()))
                .into_any_element()
        }
    }

    /// Renders the controls overlay — only visible when controls_visible = true
    fn render_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.controls_visible || self.error.is_some() || self.loading {
            return div().into_any_element();
        }

        let is_playing = self.playback == PlaybackState::Playing;
        let play_icon = if is_playing { "⏸" } else { "▶" };
        let mute_icon = if self.volume == 0.0 { "🔇" } else { "🔊" };

        let position_secs = self.position.as_secs_f64();
        let duration_secs = self.duration.as_secs_f64().max(1.0);
        let progress = (position_secs / duration_secs).clamp(0.0, 1.0) as f32;

        let pos_str = format_time(self.position);
        let dur_str = format_time(self.duration);
        let time_str = format!("{} / {}", pos_str, dur_str);

        let current_speed = self.speed;

        div()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .px_3()
            .py_2()
            .bg(gpui::rgba(0x00000088u32))
            .flex()
            .flex_col()
            .gap_2()
            // Seek bar
            .child(
                div()
                    .w_full()
                    .h(px(4.0))
                    .bg(cx.theme().colors().border)
                    .rounded_full()
                    .relative()
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .h_full()
                            .w(relative(progress))
                            .bg(cx.theme().colors().text)
                            .rounded_full()
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            // Calculate seek position from click location
                            this.last_interaction = Instant::now();
                            let click_x: f32 = event.position.x.into();
                            // We use a rough estimate here — seek proportionally
                            let ratio = (click_x / 400.0).clamp(0.0, 1.0) as f64;
                            let target_secs = ratio * duration_secs;
                            let target = Duration::from_secs_f64(target_secs);
                            if let Some(p) = &this.pipeline {
                                let _ = p.seek_simple(
                                    gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                    gst::ClockTime::from_nseconds(target.as_nanos() as u64),
                                );
                            }
                            cx.notify();
                        }),
                    )
            )
            // Bottom row: play button, time, volume, speed
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_color(gpui::white())
                    // Play/pause button
                    .child(
                        div()
                            .cursor_pointer()
                            .child(play_icon)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                                    this.toggle_play_pause(&TogglePlayPause, window, cx);
                                }),
                            )
                    )
                    // Timestamp
                    .child(div().child(time_str).text_color(gpui::white()))
                    // Spacer
                    .child(div().flex_1())
                    // Speed buttons
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .children(
                                [0.5f64, 1.0, 1.5, 2.0].iter().map(|&s| {
                                    let label = if s == 1.0 { "1×".to_string() } else { format!("{}×", s) };
                                    let is_active = (s - current_speed).abs() < 0.01;
                                    div()
                                        .px_1()
                                        .cursor_pointer()
                                        .text_color(if is_active {
                                            cx.theme().colors().text
                                        } else {
                                            cx.theme().colors().text_muted
                                        })
                                        .child(label)
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                                this.speed = s;
                                                this.last_interaction = Instant::now();
                                                if let Some(p) = &this.pipeline {
                                                    if let Some(pos) = p.query_position::<gst::ClockTime>() {
                                                        let _ = p.seek(
                                                            s,
                                                            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                                                            gst::SeekType::Set,
                                                            pos,
                                                            gst::SeekType::End,
                                                            gst::ClockTime::from_seconds(0),
                                                        );
                                                    }
                                                }
                                                cx.notify();
                                            }),
                                        )
                                })
                            )
                    )
                    // Mute button
                    .child(
                        div()
                            .cursor_pointer()
                            .child(mute_icon)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                                    this.toggle_mute(&ToggleMute, window, cx);
                                }),
                            )
                    )
            )
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// PART 4 — Zed integration traits + persistence + init()
// ---------------------------------------------------------------------------



impl EventEmitter<VideoViewEvent> for VideoView {}
impl EventEmitter<()> for VideoView {}

impl Focusable for VideoView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ---------------------------------------------------------------------------
// Item trait — controls tab title, icon, breadcrumbs
// ---------------------------------------------------------------------------
impl Item for VideoView {
    type Event = ();

    fn tab_content_text(&self, _: usize, _cx: &App) -> SharedString {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "video".to_string())
            .into()
    }

    fn tab_content(
        &self,
        params: TabContentParams,
        _window: &Window,
        cx: &App,
    ) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(params.text_color())
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(&self.path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn for_each_project_item(
        &self,
        _cx: &App,
        _f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
    }

    fn can_split(&self) -> bool {
        true
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

// ---------------------------------------------------------------------------
// ProjectItem trait — tells Zed which files we handle and how to open them
// ---------------------------------------------------------------------------
impl ProjectItem for VideoView {
    type Item = VideoItem;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Check if there is a saved position for this file
        let abs_path = item.read(cx).abs_path.clone();
        let saved_position = persistence::VideoViewerDb::global(cx)
            .get_video_position_by_path(&abs_path)
            .ok()
            .flatten()
            .map(|secs| Duration::from_secs_f64(secs));

        Self::new(item, project, saved_position, window, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        e: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView> {
        Some(InvalidItemView::new(abs_path, is_local, e, window, cx))
    }
}

// ---------------------------------------------------------------------------
// SerializableItem trait — save/restore across Zed sessions
// ---------------------------------------------------------------------------
impl SerializableItem for VideoView {
    fn serialized_item_kind() -> &'static str {
        "VideoView"
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<workspace::Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let db = persistence::VideoViewerDb::global(cx);
        window.spawn(cx, async move |cx| {
            let video_path = db
                .get_video_path(item_id, workspace_id)?
                .context("No video path found")?;

            let saved_position = db
                .get_video_position(item_id, workspace_id)
                .ok()
                .flatten()
                .map(|secs| Duration::from_secs_f64(secs));

            let (worktree, relative_path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(video_path.clone(), false, cx)
                })
                .await
                .context("Path not found")?;

            let worktree_id = worktree.update(cx, |worktree, _cx| worktree.id());

            let project_path = ProjectPath {
                worktree_id,
                path: relative_path,
            };

            let video_item = project
                .update(cx, |project, cx| {
                    project.open_path(project_path, cx)
                })
                .await?;

            let video_item = video_item
                .downcast::<VideoItem>()
                .map_err(|_| anyhow::anyhow!("Not a video item"))?;

            cx.update(|window, cx| {
                Ok(cx.new(|cx| VideoView::new(video_item, project, saved_position, window, cx)))
            })?
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = persistence::VideoViewerDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "video_viewers", &db, cx)
    }

    fn serialize(
        &mut self,
        workspace: &mut workspace::Workspace,
        item_id: ItemId,
        _closing: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let video_path = self.path.clone();
        let position_secs = self.position.as_secs_f64();

        let db = persistence::VideoViewerDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_video_path(item_id, workspace_id, video_path, position_secs)
                .await
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Persistence — SQLite database for saving video path + position
// ---------------------------------------------------------------------------
mod persistence {
    use std::path::PathBuf;
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct VideoViewerDb(ThreadSafeConnection);

    impl Domain for VideoViewerDb {
        const NAME: &str = stringify!(VideoViewerDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE IF NOT EXISTS video_viewers (
                workspace_id INTEGER,
                item_id INTEGER UNIQUE,
                video_path BLOB,
                position_secs REAL DEFAULT 0,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(VideoViewerDb, [WorkspaceDb]);

    impl VideoViewerDb {
        query! {
            pub async fn save_video_path(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                video_path: PathBuf,
                position_secs: f64
            ) -> Result<()> {
                INSERT OR REPLACE INTO video_viewers(item_id, workspace_id, video_path, position_secs)
                VALUES (?, ?, ?, ?)
            }
        }

        query! {
            pub fn get_video_path(
                item_id: ItemId,
                workspace_id: WorkspaceId
            ) -> Result<Option<PathBuf>> {
                SELECT video_path
                FROM video_viewers
                WHERE item_id = ? AND workspace_id = ?
            }
        }

        query! {
            pub fn get_video_position(
                item_id: ItemId,
                workspace_id: WorkspaceId
            ) -> Result<Option<f64>> {
                SELECT position_secs
                FROM video_viewers
                WHERE item_id = ? AND workspace_id = ?
            }
        }

        query! {
            pub fn get_video_position_by_path(
                video_path: PathBuf
            ) -> Result<Option<f64>> {
                SELECT position_secs
                FROM video_viewers
                WHERE video_path = ?
                ORDER BY item_id DESC
                LIMIT 1
            }
        }
    }
}

// ---------------------------------------------------------------------------
// init() — called from main.rs to register video_viewer with Zed
// ---------------------------------------------------------------------------
pub fn init(cx: &mut App) {
    workspace::register_project_item::<VideoView>(cx);
    workspace::register_serializable_item::<VideoView>(cx);
}

