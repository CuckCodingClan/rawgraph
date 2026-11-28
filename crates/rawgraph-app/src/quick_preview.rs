//! Selection-only display pixels. This worker never runs the graph or decodes a RAW proxy.
use std::{path::PathBuf, sync::{Arc, mpsc::{self, Receiver, Sender}}};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use rawgraph_core::{ImageBuf, ImageId};
use rawgraph_project::ThumbCache;
use rawgraph_scopes::DisplayTransform;

pub struct QuickPreview {
    tx: Sender<Job>,
    rx: Receiver<(u64, ColorImage)>,
}

pub struct Job {
    pub generation: u64,
    pub path: PathBuf,
    pub hash: String,
}

impl QuickPreview {
    pub fn new(ctx: egui::Context, cache: Option<Arc<ThumbCache>>) -> Self {
        let (tx, jobs) = mpsc::channel::<Job>();
        let (out, rx) = mpsc::channel();
        let _ = std::thread::Builder::new().name("rawgraph-quick-preview".into()).spawn(move || {
            while let Ok(job) = jobs.recv() {
                let job = jobs.try_iter().last().unwrap_or(job);
                let pixels = cache.as_ref().and_then(|cache| (!job.hash.is_empty()).then(|| cache.get(&job.hash, crate::thumbs::THUMB_CACHE_KEY)).flatten())
                    .and_then(|bytes| crate::thumbs::decode_jpeg(&bytes))
                    .or_else(|| {
                        (rawgraph_io::source_kind(&job.path) == Some(rawgraph_io::SourceKind::Raw)).then(|| rawgraph_io::raw::load_embedded_preview(&job.path, 1280).ok().flatten()).flatten()
                    });
                if let Some((w, h, rgba)) = pixels {
                    if out.send((job.generation, ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba))).is_err() { break; }
                    ctx.request_repaint();
                }
            }
        });
        Self { tx, rx }
    }

    pub fn request(&self, job: Job) { let _ = self.tx.send(job); }

    pub fn poll(&self) -> impl Iterator<Item = (u64, ColorImage)> + '_ { self.rx.try_iter() }
}

pub fn proxy_texture(ctx: &egui::Context, proxy: &ImageBuf, view: DisplayTransform) -> TextureHandle {
    // Bound UI work even when the actual cached proxy is several megapixels.
    let step = proxy.width().max(proxy.height()).div_ceil(384).max(1);
    let (w, h) = (proxy.width().div_ceil(step), proxy.height().div_ceil(step));
    let mut small = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w { small.set_pixel(x, y, proxy.pixel(x * step, y * step)); }
    }
    let (_, pixels) = view.apply_with_rgba8(&small);
    ctx.load_texture("cached proxy preview", ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &pixels), TextureOptions::LINEAR)
}

pub struct Geometry {
    pub size: [u32; 2],
    pub uv: [egui::Pos2; 4],
}

pub fn geometry(session: &crate::session::Session, node: rawgraph_core::NodeId, image: ImageId, source: (u32, u32)) -> Geometry {
    geometry_for_graph(&session.project.graph, node, image, source)
}

fn geometry_for_graph(graph: &rawgraph_core::Graph, node: rawgraph_core::NodeId, image: ImageId, source: (u32, u32)) -> Geometry {
    let mut stages = crate::frames::transforms_upstream(graph, node);
    if graph.node(node).is_some_and(|n| n.kind == rawgraph_core::NodeKind::Transform) { stages.push(node); }
    let mut size = source;
    let transforms: Vec<_> = stages.into_iter().filter_map(|id| graph.node(id)).map(|n| {
        let p = crate::tools::transform_params(n, image);
        let input = size;
        size = rawgraph_fx::transform_size(size.0, size.1, &p);
        (input, p)
    }).collect();
    let uv = [(0., 0.), (1., 0.), (1., 1.), (0., 1.)].map(|point| {
        let (x, y) = transforms.iter().rev().fold(point, |(x, y), ((w, h), p)| rawgraph_fx::transform_output_to_input(*w, *h, p, x, y));
        egui::pos2(x, y)
    });
    Geometry { size: [size.0, size.1], uv }
}

pub fn estimate_proxy_size(provider: &crate::provider::FileProvider, image: ImageId, thumbnail: (u32, u32)) -> (u32, u32) {
    let limit = provider.preview_size();
    if let Some((w, h)) = provider.full_size_of(image) {
        let bin = w.max(h).div_ceil(limit).max(1);
        return ((w / bin).max(1), (h / bin).max(1));
    }
    let (w, h) = thumbnail;
    let scale = limit as f64 / w.max(h).max(1) as f64;
    (((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1))
}

pub fn paint(painter: &egui::Painter, texture: egui::TextureId, rect: egui::Rect, uv: [egui::Pos2; 4], tint: egui::Color32) {
    let mut mesh = egui::Mesh::with_texture(texture);
    for (pos, uv) in [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom()].into_iter().zip(uv) {
        mesh.vertices.push(egui::epaint::Vertex { pos, uv, color: tint });
    }
    mesh.indices.extend([0, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upright_portrait_preview_is_rotated_and_cropped_to_the_graph_frame() {
        let mut g = rawgraph_core::Graph::default();
        let t = g.add_node(rawgraph_core::NodeKind::Transform, [0.0; 2]);
        let image = ImageId(1);
        let node = g.node_mut(t).unwrap();
        node.set_param("rotate", image, 1.0);
        node.set_param("left", image, 0.25);
        node.set_param("right", image, 0.75);
        let preview = geometry_for_graph(&g, t, image, (1200, 1600));
        assert_eq!(preview.size, [800, 1200]);
        assert_eq!(preview.uv, [egui::pos2(0.0, 0.75), egui::pos2(0.0, 0.25), egui::pos2(1.0, 0.25), egui::pos2(1.0, 0.75)]);
    }
}
