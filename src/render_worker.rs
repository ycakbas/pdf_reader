use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;

use crate::pdf::{OpenDocument, RenderedPage};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RenderJobKey {
    pub tab_id: u64,
    pub page_index: usize,
    pub is_thumbnail: bool,
}

#[derive(Debug, Clone)]
pub struct RenderJob {
    pub tab_id: u64,
    pub path: PathBuf,
    pub page_index: usize,
    pub target_width_px: i32,
    pub max_width_px: i32,
    pub lcd_text: bool,
    pub is_thumbnail: bool,
    /// Higher priority jobs (visible pages) are rendered before pre-renders and thumbnails.
    pub high_priority: bool,
}

pub enum WorkerCommand {
    Render(RenderJob),
    ClosePath(PathBuf),
}

pub struct RenderResult {
    pub tab_id: u64,
    pub page_index: usize,
    pub target_width_px: i32,
    pub is_thumbnail: bool,
    pub rendered: RenderedPage,
}

/// Dedicated background thread for PDFium page & thumbnail rasterization so the UI thread
/// never stalls during scrolling, zooming, or pre-rendering.
pub struct RenderWorker {
    tx: Sender<WorkerCommand>,
    rx: Receiver<RenderResult>,
    in_flight: HashMap<RenderJobKey, i32>,
}

impl RenderWorker {
    pub fn spawn(egui_ctx: egui::Context) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();
        let (res_tx, res_rx) = mpsc::channel::<RenderResult>();

        thread::Builder::new()
            .name("pdf_render_worker".into())
            .spawn(move || worker_loop(cmd_rx, res_tx, egui_ctx))
            .expect("failed to spawn pdf_render_worker thread");

        Self {
            tx: cmd_tx,
            rx: res_rx,
            in_flight: HashMap::new(),
        }
    }

    /// Enqueue a page or thumbnail render if an identical-resolution job isn't already in flight.
    pub fn request(&mut self, job: RenderJob) {
        let key = RenderJobKey {
            tab_id: job.tab_id,
            page_index: job.page_index,
            is_thumbnail: job.is_thumbnail,
        };
        if let Some(&queued_w) = self.in_flight.get(&key)
            && (queued_w - job.target_width_px).abs() <= 16
        {
            return;
        }
        self.in_flight.insert(key, job.target_width_px);
        let _ = self.tx.send(WorkerCommand::Render(job));
    }

    pub fn close_path(&mut self, path: PathBuf) {
        let _ = self.tx.send(WorkerCommand::ClosePath(path));
    }

    pub fn clear_in_flight_for_tab(&mut self, tab_id: u64) {
        self.in_flight.retain(|k, _| k.tab_id != tab_id);
    }

    /// Drain all completed renders ready for GPU upload on this frame.
    pub fn poll_results(&mut self) -> Vec<RenderResult> {
        let mut out = Vec::new();
        while let Ok(res) = self.rx.try_recv() {
            let key = RenderJobKey {
                tab_id: res.tab_id,
                page_index: res.page_index,
                is_thumbnail: res.is_thumbnail,
            };
            if let Some(&queued_w) = self.in_flight.get(&key)
                && queued_w == res.target_width_px
            {
                self.in_flight.remove(&key);
            }
            out.push(res);
        }
        out
    }
}

fn worker_loop(
    cmd_rx: Receiver<WorkerCommand>,
    res_tx: Sender<RenderResult>,
    egui_ctx: egui::Context,
) {
    let mut docs: HashMap<PathBuf, OpenDocument> = HashMap::new();
    let mut high_queue: VecDeque<RenderJob> = VecDeque::new();
    let mut low_queue: VecDeque<RenderJob> = VecDeque::new();
    let mut latest_width: HashMap<RenderJobKey, i32> = HashMap::new();

    loop {
        // Block waiting for work if both queues are empty
        if high_queue.is_empty() && low_queue.is_empty() {
            let Ok(first) = cmd_rx.recv() else {
                break;
            };
            ingest_command(
                first,
                &mut docs,
                &mut high_queue,
                &mut low_queue,
                &mut latest_width,
            );
        }

        // Drain any additional pending commands so we always prioritize the latest visible pages
        while let Ok(cmd) = cmd_rx.try_recv() {
            ingest_command(
                cmd,
                &mut docs,
                &mut high_queue,
                &mut low_queue,
                &mut latest_width,
            );
        }

        // Bound queue sizes if the user scrubbed wildly across hundreds of pages
        trim_queue(&mut high_queue, 12);
        trim_queue(&mut low_queue, 16);

        let Some(job) = high_queue.pop_front().or_else(|| low_queue.pop_front()) else {
            continue;
        };

        let key = RenderJobKey {
            tab_id: job.tab_id,
            page_index: job.page_index,
            is_thumbnail: job.is_thumbnail,
        };
        // Skip superseded jobs (e.g., an older zoom level for the same page)
        if let Some(&wanted_w) = latest_width.get(&key)
            && wanted_w != job.target_width_px
        {
            continue;
        }

        if !docs.contains_key(&job.path) {
            if let Ok(doc) = OpenDocument::open(&job.path) {
                docs.insert(job.path.clone(), doc);
            } else {
                continue;
            }
        }

        let Some(doc) = docs.get(&job.path) else {
            continue;
        };

        if let Ok(rendered) = doc.render_page(
            job.page_index,
            job.target_width_px,
            job.max_width_px,
            job.lcd_text,
        ) {
            let _ = res_tx.send(RenderResult {
                tab_id: job.tab_id,
                page_index: job.page_index,
                target_width_px: job.target_width_px,
                is_thumbnail: job.is_thumbnail,
                rendered,
            });
            egui_ctx.request_repaint();
        }
    }
}

fn ingest_command(
    cmd: WorkerCommand,
    docs: &mut HashMap<PathBuf, OpenDocument>,
    high_queue: &mut VecDeque<RenderJob>,
    low_queue: &mut VecDeque<RenderJob>,
    latest_width: &mut HashMap<RenderJobKey, i32>,
) {
    match cmd {
        WorkerCommand::Render(job) => {
            let key = RenderJobKey {
                tab_id: job.tab_id,
                page_index: job.page_index,
                is_thumbnail: job.is_thumbnail,
            };
            latest_width.insert(key, job.target_width_px);
            if job.high_priority {
                high_queue.push_back(job);
            } else {
                low_queue.push_back(job);
            }
        }
        WorkerCommand::ClosePath(path) => {
            docs.remove(&path);
            high_queue.retain(|j| j.path != path);
            low_queue.retain(|j| j.path != path);
        }
    }
}

fn trim_queue(queue: &mut VecDeque<RenderJob>, max_len: usize) {
    if queue.len() <= max_len {
        return;
    }
    // Keep the most recent `max_len` unique jobs
    let mut seen: HashSet<RenderJobKey> = HashSet::new();
    let mut kept = VecDeque::with_capacity(max_len);
    while let Some(job) = queue.pop_back() {
        let key = RenderJobKey {
            tab_id: job.tab_id,
            page_index: job.page_index,
            is_thumbnail: job.is_thumbnail,
        };
        if seen.insert(key) {
            kept.push_front(job);
            if kept.len() >= max_len {
                break;
            }
        }
    }
    *queue = kept;
}
