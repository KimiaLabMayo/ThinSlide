use eframe::egui;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use slide_leaner::{Args, run};
use std::{io::Read, path::PathBuf, sync::{Arc, Mutex, mpsc}, thread};

// ---------- VT100 terminal buffer -------------------------------------------

/// Minimal terminal buffer: handles \r, \n, cursor-up, erase-line, and strips
/// ANSI color/style codes so indicatif progress bars render correctly in egui.
#[derive(Default)]
struct TermBuf {
    rows: Vec<Vec<char>>,
    row:  usize,
    col:  usize,
}

impl TermBuf {
    fn ensure(&mut self) {
        while self.rows.len() <= self.row {
            self.rows.push(Vec::new());
        }
    }

    fn put(&mut self, c: char) {
        self.ensure();
        let r = &mut self.rows[self.row];
        if self.col < r.len() {
            r[self.col] = c;
        } else {
            while r.len() < self.col { r.push(' '); }
            r.push(c);
        }
        self.col += 1;
    }

    fn feed(&mut self, data: &[u8]) {
        let s = String::from_utf8_lossy(data);
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '\r' => { self.col = 0; }
                '\n' => { self.row += 1; self.col = 0; self.ensure(); }
                '\x08' => { if self.col > 0 { self.col -= 1; } }
                '\x1b' => {
                    if it.peek() == Some(&'[') {
                        it.next(); // consume '['
                        let mut p = String::new();
                        let mut cmd = ' ';
                        for ch in it.by_ref() {
                            if ch.is_ascii_alphabetic() { cmd = ch; break; }
                            p.push(ch); // collect params (digits, ';', '?')
                        }
                        let n: usize = p.chars()
                            .filter(|c| c.is_ascii_digit())
                            .collect::<String>()
                            .parse()
                            .unwrap_or(1);
                        match cmd {
                            'A' => { self.row = self.row.saturating_sub(n); }
                            'K' if p == "2" => {
                                self.ensure();
                                self.rows[self.row].clear();
                                self.col = 0;
                            }
                            _ => {} // colors, cursor visibility, alt-screen → ignore
                        }
                    } else {
                        it.next(); // skip non-CSI escape sequence byte
                    }
                }
                c if !c.is_control() => { self.put(c); }
                _ => {}
            }
        }
    }

    fn text(&self) -> String {
        self.rows.iter()
            .map(|r| r.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn clear(&mut self) { self.rows.clear(); self.row = 0; self.col = 0; }
    fn is_empty(&self) -> bool { self.rows.is_empty() }
}

// ---------- app state -------------------------------------------------------

#[derive(PartialEq, Clone, Default, serde::Serialize, serde::Deserialize)]
enum MppMode { #[default] Passthrough, Half, X20, X10 }

const STORAGE_KEY: &str = "slean_settings";

#[derive(serde::Serialize, serde::Deserialize)]
struct Settings {
    input_dir:       String,
    output_dir:      String,
    legacy:          bool,
    mpp_mode:        MppMode,
    quality:         u8,
    use_parent_name: bool,
    icc_bake:        bool,
    jobs:            String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            input_dir: String::new(), output_dir: String::new(),
            legacy: false, mpp_mode: MppMode::default(),
            quality: 87, use_parent_name: false, icc_bake: false,
            jobs: String::new(),
        }
    }
}

struct App {
    input_dir:       String,
    output_dir:      String,
    legacy:          bool,
    mpp_mode:        MppMode,
    quality:         u8,
    use_parent_name: bool,
    icc_bake:        bool,
    jobs:            String,
    term:            TermBuf,
    running:         bool,
    completion:      Option<String>,
    rx:              Option<mpsc::Receiver<Vec<u8>>>,
    kill_child:      Option<Arc<Mutex<Box<dyn portable_pty::Child + Send>>>>,
}

impl Default for App {
    fn default() -> Self {
        let mut term = TermBuf::default();
        term.feed(concat!(
            "Slide Leaner — high-throughput WSI optimizer\n",
            "─────────────────────────────────────────────────────────\n",
            "\n",
            "Features:\n",
            "  • DICOM → TIFF lossless conversion  (zero quality loss)\n",
            "  • Downsampling  (half / 20x / 10x)\n",
            "  • ICC profile baking  (converts to sRGB, removes embedded profile)\n",
            "\n",
            "Supported input formats:  DICOM, SVS, TIFF, OME-TIFF\n",
            "Output formats:           OME-TIFF, generic TIFF/SVS (OpenSlide-compatible)\n",
            "\n",
            "Select input/output folders above, configure options, then click ▶ Run.\n",
        ).as_bytes());
        Self {
            input_dir:       String::new(),
            output_dir:      String::new(),
            legacy:          false,
            mpp_mode:        MppMode::Passthrough,
            quality:         87,
            use_parent_name: false,
            icc_bake:        false,
            jobs:            String::new(),
            term,
            running:         false,
            completion:      None,
            rx:              None,
            kill_child:      None,
        }
    }
}

// ---------- eframe::App -----------------------------------------------------

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let s: Settings = cc.storage
            .and_then(|st| eframe::get_value(st, STORAGE_KEY))
            .unwrap_or_default();
        let mut app = App::default();
        app.input_dir       = s.input_dir;
        app.output_dir      = s.output_dir;
        app.legacy          = s.legacy;
        app.mpp_mode        = s.mpp_mode;
        app.quality         = s.quality;
        app.use_parent_name = s.use_parent_name;
        app.icc_bake        = s.icc_bake;
        app.jobs            = s.jobs;
        app
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, STORAGE_KEY, &Settings {
            input_dir:       self.input_dir.clone(),
            output_dir:      self.output_dir.clone(),
            legacy:          self.legacy,
            mpp_mode:        self.mpp_mode.clone(),
            quality:         self.quality,
            use_parent_name: self.use_parent_name,
            icc_bake:        self.icc_bake,
            jobs:            self.jobs.clone(),
        });
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain bytes from the PTY reader thread
        if let Some(rx) = &self.rx {
            let mut done = false;
            loop {
                match rx.try_recv() {
                    Ok(data) if data.is_empty() => { done = true; break; }
                    Ok(data) => { self.term.feed(&data); }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => { done = true; break; }
                }
            }
            if done {
                self.running = false;
                self.rx = None;
                self.completion = Some(if let Some(c) = self.kill_child.take() {
                    let ok = c.lock().unwrap().wait()
                        .map(|s| s.success()).unwrap_or(false);
                    if ok { "✓  Completed".to_string() } else { "✗  Failed".to_string() }
                } else {
                    "■  Stopped".to_string()
                });
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {

            let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
                i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
            });
            for path in dropped {
                if path.is_dir() {
                    if self.input_dir.is_empty() {
                        self.input_dir = path.display().to_string();
                    } else {
                        self.output_dir = path.display().to_string();
                    }
                }
            }
            if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
                ui.colored_label(
                    egui::Color32::from_rgb(60, 140, 220),
                    "↓  Drop a folder to set input path (or output path if input is already set)",
                );
            }

            egui::Grid::new("folders").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Input folder:");
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Browse…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                self.input_dir = p.display().to_string();
                            }
                        }
                        ui.add(egui::TextEdit::singleline(&mut self.input_dir)
                            .desired_width(f32::INFINITY));
                    });
                });
                ui.end_row();

                ui.label("Output folder:");
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Browse…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                self.output_dir = p.display().to_string();
                            }
                        }
                        ui.add(egui::TextEdit::singleline(&mut self.output_dir)
                            .desired_width(f32::INFINITY));
                    });
                });
                ui.end_row();
            });

            ui.add_space(4.0);
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Output format:");
                ui.radio_value(&mut self.legacy, false, "OME-TIFF");
                ui.radio_value(&mut self.legacy, true, "generic tiff/svs, openslide-compatible");
            });

            ui.horizontal(|ui| {
                ui.label("Downsampling:");
                ui.radio_value(&mut self.mpp_mode, MppMode::Passthrough, "None");
                ui.radio_value(&mut self.mpp_mode, MppMode::Half, "Half in each dimension");
                ui.radio_value(&mut self.mpp_mode, MppMode::X20, "20x (0.5 mpp)");
                ui.radio_value(&mut self.mpp_mode, MppMode::X10, "10x (1.0 mpp)");
            });

            if matches!(self.mpp_mode, MppMode::Half | MppMode::X20 | MppMode::X10) {
                ui.horizontal(|ui| {
                    ui.label("JPEG quality:");
                    ui.add(egui::Slider::new(&mut self.quality, 30_u8..=100_u8));
                });
            }

            ui.checkbox(&mut self.use_parent_name, "Use parent name as filename (DICOM only)");
            ui.checkbox(&mut self.icc_bake, "Convert to sRGB color space and remove ICC profile (slow)");

            ui.horizontal(|ui| {
                ui.label("Jobs:");
                ui.add(egui::TextEdit::singleline(&mut self.jobs).desired_width(55.0));
                ui.label("(blank = auto)");
            });

            ui.add_space(4.0);
            ui.separator();

            ui.horizontal(|ui| {
                let can_run = !self.input_dir.is_empty()
                    && !self.output_dir.is_empty()
                    && !self.running;
                if ui.add_enabled(can_run, egui::Button::new("▶  Run")).clicked() {
                    self.start(ctx.clone());
                }
                if self.running {
                    ui.spinner();
                    ui.label("Running…");
                    if ui.button("■  Stop").clicked() {
                        if let Some(c) = self.kill_child.take() {
                            let _ = c.lock().unwrap().kill();
                        }
                    }
                }
                if !self.running {
                    if let Some(msg) = &self.completion {
                        let color = if msg.starts_with('✓') {
                            egui::Color32::from_rgb(60, 180, 60)
                        } else if msg.starts_with('✗') {
                            egui::Color32::from_rgb(210, 50, 50)
                        } else {
                            egui::Color32::from_rgb(200, 160, 0)
                        };
                        ui.label(egui::RichText::new(msg).strong().color(color));
                    }
                    if !self.term.is_empty() {
                        if ui.button("Clear log").clicked() {
                            self.term.clear();
                            self.completion = None;
                        }
                    }
                }
            });

            ui.separator();

            let avail = ui.available_rect_before_wrap();
            let mut display = self.term.text();
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add_sized(
                        avail.size(),
                        egui::TextEdit::multiline(&mut display)
                            .font(egui::TextStyle::Monospace),
                    );
                });
        });
    }
}

// ---------- App::start ------------------------------------------------------

impl App {
    fn start(&mut self, ctx: egui::Context) {
        self.term.clear();
        self.running = true;
        self.completion = None;

        let slean = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("slean-gui"));

        let mut cmd = CommandBuilder::new(&slean);
        cmd.arg(&self.input_dir);
        cmd.arg(&self.output_dir);
        if self.legacy { cmd.arg("--legacy"); }
        match &self.mpp_mode {
            MppMode::Half => {
                cmd.arg("--half");
                cmd.arg("--quality"); cmd.arg(self.quality.to_string());
            }
            MppMode::X20 => {
                cmd.arg("--mpp"); cmd.arg("0.5");
                cmd.arg("--quality"); cmd.arg(self.quality.to_string());
            }
            MppMode::X10 => {
                cmd.arg("--mpp"); cmd.arg("1.0");
                cmd.arg("--quality"); cmd.arg(self.quality.to_string());
            }
            MppMode::Passthrough => {}
        }
        if self.use_parent_name { cmd.arg("--use-parent-name"); }
        if self.icc_bake { cmd.arg("--icc-bake"); }
        if let Ok(n) = self.jobs.parse::<usize>() {
            if n > 0 { cmd.arg("--jobs"); cmd.arg(n.to_string()); }
        }

        // Open a PTY so indicatif detects a terminal and renders progress bars
        let pty_system = native_pty_system();
        let pair = match pty_system.openpty(PtySize { rows: 40, cols: 120,
                                                       pixel_width: 0, pixel_height: 0 }) {
            Ok(p) => p,
            Err(e) => {
                self.term.feed(format!("PTY error: {e}").as_bytes());
                self.running = false;
                return;
            }
        };

        let child = match pair.slave.spawn_command(cmd) {
            Ok(c) => c,
            Err(e) => {
                self.term.feed(format!("Failed to start slean: {e}").as_bytes());
                self.running = false;
                return;
            }
        };
        drop(pair.slave);

        // Store child so the Stop button can kill it
        self.kill_child = Some(Arc::new(Mutex::new(child)));

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        self.rx = Some(rx);

        let master = pair.master;
        thread::spawn(move || {
            let mut reader = master.try_clone_reader().expect("PTY reader");
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = tx.send(buf[..n].to_vec());
                        ctx.request_repaint();
                    }
                }
            }
            drop(reader);
            drop(master);
            let _ = tx.send(Vec::new()); // empty vec = done signal
            ctx.request_repaint();
        });
    }
}

// ---------- main ------------------------------------------------------------

fn main() -> eframe::Result<()> {
    let raw: Vec<String> = std::env::args().collect();
    // Positional args present → CLI mode (no GUI window)
    if raw[1..].iter().any(|a| !a.starts_with('-')) {
        let start_time = std::time::Instant::now();
        let args = Args::build(raw.into_iter()).unwrap_or_else(|err| {
            eprintln!("Problem parsing arguments: {err}");
            std::process::exit(1);
        });
        run(args);
        println!("Total execution time: {:.2?}", start_time.elapsed());
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Slide Leaner")
            .with_inner_size([680.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("Slide Leaner", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
