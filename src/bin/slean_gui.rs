use eframe::egui;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{io::Read, path::PathBuf, sync::mpsc, thread};

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

#[derive(PartialEq, Clone, Default)]
enum MppMode { #[default] Passthrough, Half, Custom }

struct App {
    input_dir:       String,
    output_dir:      String,
    legacy:          bool,
    mpp_mode:        MppMode,
    mpp_value:       String,
    quality:         u8,
    use_parent_name: bool,
    icc_bake:        bool,
    jobs:            String,
    term:            TermBuf,
    running:         bool,
    rx:              Option<mpsc::Receiver<Vec<u8>>>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            input_dir:       String::new(),
            output_dir:      String::new(),
            legacy:          false,
            mpp_mode:        MppMode::Passthrough,
            mpp_value:       String::new(),
            quality:         87,
            use_parent_name: false,
            icc_bake:        false,
            jobs:            String::new(),
            term:            TermBuf::default(),
            running:         false,
            rx:              None,
        }
    }
}

// ---------- eframe::App -----------------------------------------------------

impl eframe::App for App {
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
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Slide Leaner");
            ui.separator();

            egui::Grid::new("folders").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Input:");
                ui.add(egui::TextEdit::singleline(&mut self.input_dir).desired_width(420.0));
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.input_dir = p.display().to_string();
                    }
                }
                ui.end_row();

                ui.label("Output:");
                ui.add(egui::TextEdit::singleline(&mut self.output_dir).desired_width(420.0));
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.output_dir = p.display().to_string();
                    }
                }
                ui.end_row();
            });

            ui.add_space(4.0);
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Format:");
                ui.radio_value(&mut self.legacy, false, "OME-TIFF");
                ui.radio_value(&mut self.legacy, true, "Legacy (BigTIFF/SVS)");
            });

            ui.horizontal(|ui| {
                ui.label("Downsampling:");
                ui.radio_value(&mut self.mpp_mode, MppMode::Passthrough, "Passthrough");
                ui.radio_value(&mut self.mpp_mode, MppMode::Half, "Half");
                ui.radio_value(&mut self.mpp_mode, MppMode::Custom, "MPP:");
                if self.mpp_mode == MppMode::Custom {
                    ui.add(egui::TextEdit::singleline(&mut self.mpp_value).desired_width(55.0));
                    ui.label("µm/px");
                }
            });

            if matches!(self.mpp_mode, MppMode::Half | MppMode::Custom) {
                ui.horizontal(|ui| {
                    ui.label("Quality:");
                    ui.add(egui::Slider::new(&mut self.quality, 1_u8..=100_u8));
                });
            }

            ui.checkbox(&mut self.use_parent_name, "Use parent name");
            ui.checkbox(&mut self.icc_bake, "ICC bake (convert to sRGB)");

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
                }
                if !self.running && !self.term.is_empty() {
                    if ui.button("Clear log").clicked() {
                        self.term.clear();
                    }
                }
            });

            ui.separator();

            let height = ui.available_height();
            let mut display = self.term.text();
            egui::ScrollArea::vertical()
                .max_height(height)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut display)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY),
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

        let slean = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("slean")))
            .unwrap_or_else(|| PathBuf::from("slean"));

        let mut cmd = CommandBuilder::new(&slean);
        cmd.arg(&self.input_dir);
        cmd.arg(&self.output_dir);
        if self.legacy { cmd.arg("--legacy"); }
        match &self.mpp_mode {
            MppMode::Half => {
                cmd.arg("--half");
                cmd.arg("--quality"); cmd.arg(self.quality.to_string());
            }
            MppMode::Custom => {
                if !self.mpp_value.is_empty() {
                    cmd.arg("--mpp"); cmd.arg(&self.mpp_value);
                }
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
            drop(child);
            let _ = tx.send(Vec::new()); // empty vec = done signal
            ctx.request_repaint();
        });
    }
}

// ---------- main ------------------------------------------------------------

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Slide Leaner")
            .with_inner_size([680.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("Slide Leaner", options, Box::new(|_cc| Ok(Box::new(App::default()))))
}
