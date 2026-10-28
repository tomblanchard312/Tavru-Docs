//! Generate an image, a logo, or a generative expand through the SharedAI daemon.
//!
//! The call runs on a worker thread. Each frame polls the result and asks for another frame
//! while it is still running. An expand replaces the selected picture.

use serde_json::json;
use std::sync::mpsc::{self, Receiver};

use crate::WordApp;

const APP: &str = "nisaba";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Task {
    Image,
    Logo,
    Expand,
}

pub struct State {
    open: bool,
    task: Task,
    prompt: String,
    negative: String,
    model: String,
    models: Vec<String>,
    width: String,
    height: String,
    steps: String,
    expand_left: String,
    expand_right: String,
    expand_top: String,
    expand_bottom: String,
    /// Media key to replace when an expand finishes, if that picture is still selected.
    replace: Option<String>,
    status: String,
    /// Download bytes or sampling steps for the job in `rx`.
    meter: Option<Meter>,
    rx: Option<Receiver<Outcome>>,
}

#[derive(Clone, Copy)]
enum Meter {
    Download { done: u64, total: u64 },
    Draw { step: u32, total: u32 },
}

enum Outcome {
    Models(Result<Vec<String>, String>),
    Picture(Result<Generated, String>),
    Progress { done: u64, total: u64 },
    Step { step: u32, total: u32 },
    Downloaded { registered: bool, message: String },
}

struct Generated {
    png: Vec<u8>,
    width: u32,
    height: u32,
    seed: i64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            open: false,
            task: Task::Image,
            prompt: String::new(),
            negative: String::new(),
            model: initial_model(),
            models: Vec::new(),
            width: initial_side(),
            height: initial_side(),
            steps: initial_steps(),
            expand_left: "128".into(),
            expand_right: "128".into(),
            expand_top: "128".into(),
            expand_bottom: "128".into(),
            replace: None,
            status: String::new(),
            meter: None,
            rx: None,
        }
    }
}

pub fn open(app: &mut WordApp, task: &str) {
    app.image_gen.open = true;
    app.image_gen.task = match task {
        "logo" => Task::Logo,
        "expand" => Task::Expand,
        _ => Task::Image,
    };
    if app.image_gen.models.is_empty() && app.image_gen.rx.is_none() {
        refresh(app);
    }
}

pub fn show(app: &mut WordApp, ctx: &egui::Context) {
    if let Some(outcome) = poll(&mut app.image_gen, ctx) {
        apply(app, outcome);
    }
    if !app.image_gen.open {
        return;
    }
    let mut open = true;
    let title = match app.image_gen.task {
        Task::Image => tl!("Generate image"),
        Task::Logo => tl!("Generate logo"),
        Task::Expand => tl!("Expand picture"),
    };
    let busy = app.image_gen.rx.is_some();
    egui::Window::new(title).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -40.0)).open(&mut open).show(
        ctx,
        |ui| {
            ui.set_min_width(440.0);
            body(app, ui);
        },
    );
    if busy {
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    } else if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.image_gen.open = false;
    }
}

fn body(app: &mut WordApp, ui: &mut egui::Ui) {
    let busy = app.image_gen.rx.is_some();
    if busy || !app.image_gen.status.is_empty() {
        let status = app.image_gen.status.clone();
        let meter = app.image_gen.meter;
        notice(ui, busy, &status, meter.as_ref());
    }
    ui.add_enabled_ui(!busy, |ui| {
        fields(app, ui);
    });
    ui.horizontal(|ui| {
        if ui.add_enabled(!busy, egui::Button::new(tl!("Generate"))).clicked() {
            generate(app);
        }
        if ui.add_enabled(!busy, egui::Button::new(tl!("Close"))).clicked() {
            app.image_gen.open = false;
        }
    });
}

fn notice(ui: &mut egui::Ui, busy: bool, status: &str, meter: Option<&Meter>) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            if busy {
                ui.add(egui::Spinner::new().size(20.0));
            }
            let text = if status.is_empty() { "Working…" } else { status };
            ui.label(egui::RichText::new(tl!(text)).strong());
        });
        match meter {
            Some(Meter::Download { done, total }) => {
                let frac = *done as f32 / (*total).max(1) as f32;
                ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).show_percentage());
            }
            Some(Meter::Draw { step, total }) => {
                let frac = *step as f32 / (*total).max(1) as f32;
                ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).text(format!("{step} / {total}")));
            }
            None => {}
        }
    });
}

fn fields(app: &mut WordApp, ui: &mut egui::Ui) {
    let busy = app.image_gen.rx.is_some();
    ui.horizontal(|ui| {
        ui.selectable_value(&mut app.image_gen.task, Task::Image, tl!("Image"));
        ui.selectable_value(&mut app.image_gen.task, Task::Logo, tl!("Logo"));
        ui.selectable_value(&mut app.image_gen.task, Task::Expand, tl!("Expand"));
    });
    ui.label(match app.image_gen.task {
        Task::Image => tl!("Describe the picture. To draw words, put them in quotes."),
        Task::Logo => tl!("Describe the mark. The logo wording is added for you."),
        Task::Expand => tl!("Describe what to paint in the new margin. The selected picture stays."),
    });
    if app.image_gen.task == Task::Expand && app.selected_picture_media().is_none() {
        ui.label(tl!("Select a picture to expand."));
    }
    ui.add(egui::TextEdit::multiline(&mut app.image_gen.prompt).desired_rows(3).hint_text(tl!("a heron in profile")));
    ui.label(tl!("Negative prompt"));
    ui.add(egui::TextEdit::singleline(&mut app.image_gen.negative).hint_text(tl!("optional")));
    model_row(app, ui, busy);
    if app.image_gen.task == Task::Expand {
        ui.label(tl!("Margins, in pixels"));
        ui.horizontal(|ui| {
            ui.label(tl!("Left"));
            ui.add(egui::TextEdit::singleline(&mut app.image_gen.expand_left).desired_width(48.0));
            ui.label(tl!("Right"));
            ui.add(egui::TextEdit::singleline(&mut app.image_gen.expand_right).desired_width(48.0));
            ui.label(tl!("Top"));
            ui.add(egui::TextEdit::singleline(&mut app.image_gen.expand_top).desired_width(48.0));
            ui.label(tl!("Bottom"));
            ui.add(egui::TextEdit::singleline(&mut app.image_gen.expand_bottom).desired_width(48.0));
        });
    } else {
        ui.horizontal(|ui| {
            ui.label(tl!("Width"));
            ui.add(egui::TextEdit::singleline(&mut app.image_gen.width).desired_width(64.0));
            ui.label(tl!("Height"));
            ui.add(egui::TextEdit::singleline(&mut app.image_gen.height).desired_width(64.0));
            ui.label(tl!("Steps"));
            ui.add(egui::TextEdit::singleline(&mut app.image_gen.steps).desired_width(48.0));
        });
        ui.label(tl!("FLUX.2 klein 4B uses 4 steps at 1024 and can draw words. Other models often want 20 steps at 512."));
    }
}

fn model_row(app: &mut WordApp, ui: &mut egui::Ui, busy: bool) {
    ui.horizontal(|ui| {
        let selected = if app.image_gen.model.is_empty() { tl!("Model").to_string() } else { app.image_gen.model.clone() };
        egui::ComboBox::from_id_salt("nisaba-image-model").selected_text(selected).show_ui(ui, |ui| {
            for id in app.image_gen.models.clone() {
                ui.selectable_value(&mut app.image_gen.model, id.clone(), id);
            }
        });
        if ui.add_enabled(!busy, egui::Button::new(tl!("Refresh"))).clicked() {
            refresh(app);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if !sharedai_client::default_image_model_present() && ui.add_enabled(!busy, egui::Button::new(tl!("Download FLUX.2 klein 4B"))).clicked() {
            start_download(app);
        }
    });
    ui.horizontal(|ui| {
        ui.label(tl!("Model id"));
        ui.add(egui::TextEdit::singleline(&mut app.image_gen.model).desired_width(240.0));
    });
}

fn generate(app: &mut WordApp) {
    #[cfg(target_arch = "wasm32")]
    {
        app.image_gen.status = "Image generation runs in the desktop app.".into();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Some(request) = request(app) else { return };
        let (tx, rx) = mpsc::channel();
        app.image_gen.rx = Some(rx);
        app.image_gen.meter = None;
        app.image_gen.status = "Loading the image model. This can take a few minutes.".into();
        std::thread::spawn(move || {
            let result = sharedai_client::generate_image_with_progress(APP, request, |step, total| {
                let _ = tx.send(Outcome::Step { step, total });
            })
            .map(|picture| Generated { png: picture.png, width: picture.width, height: picture.height, seed: picture.seed })
            .map_err(|e| e.to_string());
            let _ = tx.send(Outcome::Picture(result));
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn request(app: &mut WordApp) -> Option<sharedai_client::ImageRequest> {
    if app.image_gen.model.trim().is_empty() {
        app.image_gen.status = "Name a diffusion model.".into();
        return None;
    }
    if app.image_gen.model.trim() == sharedai_client::DEFAULT_IMAGE_MODEL_ID && !sharedai_client::default_image_model_present() {
        app.image_gen.status = "Download FLUX.2 klein 4B first. It is about 5.1 GB.".into();
        return None;
    }
    if app.image_gen.prompt.trim().is_empty() {
        app.image_gen.status = "Write a prompt first.".into();
        return None;
    }
    let model = app.image_gen.model.trim().to_string();
    let prompt = app.image_gen.prompt.trim().to_string();
    let negative = app.image_gen.negative.trim().to_string();
    let mut request = match app.image_gen.task {
        Task::Image | Task::Logo => {
            app.image_gen.replace = None;
            let width = match side(&app.image_gen.width) {
                Ok(n) => n,
                Err(e) => {
                    app.image_gen.status = e;
                    return None;
                }
            };
            let height = match side(&app.image_gen.height) {
                Ok(n) => n,
                Err(e) => {
                    app.image_gen.status = e;
                    return None;
                }
            };
            let steps = match whole(&app.image_gen.steps, "steps") {
                Ok(n) => n,
                Err(e) => {
                    app.image_gen.status = e;
                    return None;
                }
            };
            let mut request = match app.image_gen.task {
                Task::Logo => sharedai_client::ImageRequest::logo(model, prompt),
                _ => sharedai_client::ImageRequest::image(model, prompt),
            };
            request.width = width;
            request.height = height;
            request.steps = steps;
            request
        }
        Task::Expand => {
            let left = match margin(&app.image_gen.expand_left) {
                Ok(n) => n,
                Err(e) => {
                    app.image_gen.status = e;
                    return None;
                }
            };
            let right = match margin(&app.image_gen.expand_right) {
                Ok(n) => n,
                Err(e) => {
                    app.image_gen.status = e;
                    return None;
                }
            };
            let top = match margin(&app.image_gen.expand_top) {
                Ok(n) => n,
                Err(e) => {
                    app.image_gen.status = e;
                    return None;
                }
            };
            let bottom = match margin(&app.image_gen.expand_bottom) {
                Ok(n) => n,
                Err(e) => {
                    app.image_gen.status = e;
                    return None;
                }
            };
            if left + right + top + bottom == 0 {
                app.image_gen.status = "An expand needs a margin on at least one side.".into();
                return None;
            }
            let Some(media) = app.selected_picture_media() else {
                app.image_gen.status = "Select a picture to expand.".into();
                return None;
            };
            let Some(bytes) = app.session.doc.media.get(&media).cloned() else {
                app.image_gen.status = "The selected picture has no image data.".into();
                return None;
            };
            let png = match fit_png(bytes.as_slice(), left, right, top, bottom) {
                Ok(png) => png,
                Err(e) => {
                    app.image_gen.status = e;
                    return None;
                }
            };
            app.image_gen.replace = Some(media);
            let mut request = sharedai_client::ImageRequest::expand(model, prompt, png);
            request.expand_left = left;
            request.expand_right = right;
            request.expand_top = top;
            request.expand_bottom = bottom;
            request
        }
    };
    request.negative = negative;
    if request.model == sharedai_client::DEFAULT_IMAGE_MODEL_ID {
        request.cfg = sharedai_client::DEFAULT_IMAGE_CFG;
    }
    Some(request)
}

fn refresh(app: &mut WordApp) {
    #[cfg(target_arch = "wasm32")]
    {
        app.image_gen.status = "Image generation runs in the desktop app.".into();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if app.image_gen.rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        app.image_gen.rx = Some(rx);
        app.image_gen.meter = None;
        app.image_gen.status = "Looking for diffusion models…".into();
        std::thread::spawn(move || {
            let result = sharedai_client::diffusion_models(APP).map_err(|e| e.to_string());
            let _ = tx.send(Outcome::Models(result));
        });
    }
}

fn apply(app: &mut WordApp, outcome: Outcome) {
    if !app.image_gen.open {
        return;
    }
    match outcome {
        Outcome::Models(Ok(ids)) => {
            app.image_gen.models = ids;
            if app.image_gen.model.is_empty()
                && let Some(first) = app.image_gen.models.first()
            {
                app.image_gen.model = first.clone();
            }
            app.image_gen.status = if app.image_gen.models.is_empty() {
                "FLUX.2 klein 4B is the default. Download it (about 5.1 GB, Apache 2.0) or name another diffusion model.".into()
            } else {
                String::new()
            };
        }
        Outcome::Models(Err(e)) => app.image_gen.status = e,
        Outcome::Progress { .. } | Outcome::Step { .. } => {}
        Outcome::Downloaded { registered, message } => {
            app.image_gen.status = message;
            if registered {
                refresh(app);
            }
        }
        Outcome::Picture(Ok(picture)) => {
            let data = wordcraft_engine::cmd::insert::base64_encode(&picture.png);
            let replace = app.image_gen.replace.clone();
            let result = if replace.is_some() && replace == app.selected_picture_media() {
                app.run("picture.change", json!({"data": data}))
            } else {
                app.run("insert.picture", json!({"data": data}))
            };
            app.image_gen.replace = None;
            match result {
                Ok(_) => {
                    app.image_gen.status = format!("Placed a {}×{} image (seed {}).", picture.width, picture.height, picture.seed);
                }
                Err(e) => app.image_gen.status = e,
            }
        }
        Outcome::Picture(Err(e)) => {
            app.image_gen.replace = None;
            app.image_gen.status = e;
        }
    }
}

fn poll(state: &mut State, ctx: &egui::Context) -> Option<Outcome> {
    let rx = state.rx.as_ref()?;
    loop {
        match rx.try_recv() {
            Ok(Outcome::Progress { done, total }) => {
                state.status = download_status(done, total);
                state.meter = Some(Meter::Download { done, total });
                ctx.request_repaint();
            }
            Ok(Outcome::Step { step, total }) => {
                state.status = format!("Drawing, step {step} of {total}.");
                state.meter = Some(Meter::Draw { step, total });
                ctx.request_repaint();
            }
            Ok(outcome) => {
                state.rx = None;
                state.meter = None;
                return Some(outcome);
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint();
                return None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                state.rx = None;
                state.meter = None;
                return Some(Outcome::Picture(Err("image generation stopped".into())));
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn start_download(app: &mut WordApp) {
    if app.image_gen.rx.is_some() {
        return;
    }
    let (tx, rx) = mpsc::channel();
    app.image_gen.rx = Some(rx);
    app.image_gen.meter = None;
    app.image_gen.status = "Downloading FLUX.2 klein 4B…".into();
    std::thread::spawn(move || {
        let result = sharedai_client::download_default_image_model(|done, total| {
            let _ = tx.send(Outcome::Progress { done, total });
        });
        let outcome = match result {
            Ok(report) => Outcome::Downloaded { registered: report.registered, message: report.message },
            Err(e) => Outcome::Downloaded { registered: false, message: e.to_string() },
        };
        let _ = tx.send(outcome);
    });
}

fn initial_model() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        sharedai_client::DEFAULT_IMAGE_MODEL_ID.into()
    }
    #[cfg(target_arch = "wasm32")]
    {
        String::new()
    }
}

fn initial_side() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        sharedai_client::DEFAULT_IMAGE_SIDE.to_string()
    }
    #[cfg(target_arch = "wasm32")]
    {
        "512".into()
    }
}

fn initial_steps() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        sharedai_client::DEFAULT_IMAGE_STEPS.to_string()
    }
    #[cfg(target_arch = "wasm32")]
    {
        "20".into()
    }
}

fn download_status(done: u64, total: u64) -> String {
    fn gb(n: u64) -> String {
        format!("{:.1} GB", n as f64 / 1_000_000_000.0)
    }
    format!("Downloading FLUX.2 klein 4B… {} of {}", gb(done), gb(total))
}

#[cfg(not(target_arch = "wasm32"))]
fn side(text: &str) -> Result<u32, String> {
    let n = whole(text, "size")?;
    if n != 0 && (!(64..=1024).contains(&n) || n % 8 != 0) {
        return Err("width and height must be 64..=1024 and a multiple of 8".into());
    }
    Ok(n)
}

#[cfg(not(target_arch = "wasm32"))]
fn margin(text: &str) -> Result<u32, String> {
    let n = whole(text, "margin")?;
    if n > sharedai_client::limits::MAX_EXPAND_PIXELS {
        return Err("each margin must be 512 pixels or less".into());
    }
    Ok(n)
}

#[cfg(not(target_arch = "wasm32"))]
fn whole(text: &str, name: &str) -> Result<u32, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    text.parse().map_err(|_| format!("{name} must be a whole number"))
}

/// Scale and re-encode the selected picture so the expanded canvas fits the protocol.
#[cfg(not(target_arch = "wasm32"))]
fn fit_png(bytes: &[u8], left: u32, right: u32, top: u32, bottom: u32) -> Result<Vec<u8>, String> {
    use sharedai_client::limits;
    let img = image::load_from_memory(bytes).map_err(|e| format!("cannot read the picture: {e}"))?;
    let max_w = limits::MAX_IMAGE_SIDE.saturating_sub(left.saturating_add(right));
    let max_h = limits::MAX_IMAGE_SIDE.saturating_sub(top.saturating_add(bottom));
    if max_w < 1 || max_h < 1 {
        return Err("the margins leave no room for the picture".into());
    }
    let (mut w, mut h) = (img.width().min(max_w), img.height().min(max_h));
    if img.width() > max_w || img.height() > max_h {
        let scale = (max_w as f32 / img.width() as f32).min(max_h as f32 / img.height() as f32);
        w = ((img.width() as f32) * scale).floor().max(1.0) as u32;
        h = ((img.height() as f32) * scale).floor().max(1.0) as u32;
    }
    let mut current = img;
    loop {
        let canvas_w = w.saturating_add(left).saturating_add(right);
        let canvas_h = h.saturating_add(top).saturating_add(bottom);
        if canvas_w < limits::MIN_IMAGE_SIDE || canvas_h < limits::MIN_IMAGE_SIDE {
            return Err("the expanded image must be at least 64 pixels on a side".into());
        }
        let resized = if current.width() == w && current.height() == h {
            current.clone()
        } else {
            current.resize_exact(w, h, image::imageops::FilterType::Triangle)
        };
        let mut png = Vec::new();
        resized.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).map_err(|e| format!("cannot encode the picture: {e}"))?;
        if png.len() <= limits::MAX_SOURCE_PNG_BYTES {
            return Ok(png);
        }
        if w <= 64 && h <= 64 {
            return Err("the picture is too large to expand".into());
        }
        w = (w * 3 / 4).max(1);
        h = (h * 3 / 4).max(1);
        current = resized;
    }
}
