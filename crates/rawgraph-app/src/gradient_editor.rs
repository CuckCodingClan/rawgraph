//! Dual independent color and opacity stop editor for Generator gradients.
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use rawgraph_core::{ColorStop, GradientRamp, ImageId, Node, NodeData, NodeId, OpacityStop};
use crate::session::Session;

#[derive(Clone, Copy, Default)]
struct State { lane: usize, selected: Option<usize>, dragging: bool }
fn key(id: NodeId, tag: &str) -> egui::Id { egui::Id::unique(("gradient", id.0, tag)) }

// Preserve older scalar ramps until the first edit, then materialize the equivalent structured ramp.
fn ramp_of(n: &Node, image: ImageId) -> GradientRamp {
    if let Some(NodeData::GradientRamp(r)) = n.data(image) { return r.clone(); }
    let p = |name: &str| n.param(name, image);
    let c = |prefix: &str| [p(&format!("{prefix}_r")), p(&format!("{prefix}_g")), p(&format!("{prefix}_b"))];
    let mut r = GradientRamp {
        colors: vec![ColorStop { position: 0., color: c("start"), midpoint: p("ramp_color_midpoint_1") / 100. }, ColorStop { position: 1., color: c("end"), midpoint: 0.5 }],
        opacities: vec![OpacityStop { position: 0., opacity: p("ramp_alpha_start") / 100., midpoint: p("ramp_alpha_midpoint_1") / 100. }, OpacityStop { position: 1., opacity: p("ramp_alpha_end") / 100., midpoint: 0.5 }],
    };
    let cp = p("ramp_color_position") / 100.;
    if n.toggle("ramp_middle_color", image) && cp > 0. && cp < 1. { r.colors.insert(1, ColorStop { position: cp, color: c("ramp_color"), midpoint: p("ramp_color_midpoint_2") / 100. }); }
    if n.toggle("ramp_middle_color", image) && cp <= 0. { r.colors[0].color = c("ramp_color"); r.colors[0].midpoint = p("ramp_color_midpoint_2") / 100.; }
    if n.toggle("ramp_middle_color", image) && cp >= 1. { r.colors[1].color = c("ramp_color"); }
    let ap = p("ramp_alpha_position") / 100.;
    if n.toggle("ramp_middle_alpha", image) && ap > 0. && ap < 1. { r.opacities.insert(1, OpacityStop { position: ap, opacity: p("ramp_alpha_middle") / 100., midpoint: p("ramp_alpha_midpoint_2") / 100. }); }
    if n.toggle("ramp_middle_alpha", image) && ap <= 0. { r.opacities[0].opacity = p("ramp_alpha_middle") / 100.; r.opacities[0].midpoint = p("ramp_alpha_midpoint_2") / 100.; }
    if n.toggle("ramp_middle_alpha", image) && ap >= 1. { r.opacities[1].opacity = p("ramp_alpha_middle") / 100.; }
    r
}
fn positions(r: &GradientRamp, lane: usize) -> Vec<f32> { if lane == 0 { r.colors.iter().map(|s| s.position).collect() } else { r.opacities.iter().map(|s| s.position).collect() } }
fn move_stop(r: &mut GradientRamp, lane: usize, i: usize, target: f32) {
    let ps = positions(r, lane); let lo = if i == 0 { 0. } else { ps[i-1] + 0.00001 }; let hi = if i+1 == ps.len() { 1. } else { ps[i+1] - 0.00001 };
    if lo <= hi { if lane == 0 { r.colors[i].position = target.clamp(lo, hi); } else { r.opacities[i].position = target.clamp(lo, hi); } }
}
fn add_stop(r: &mut GradientRamp, lane: usize, t: f32, smooth: f32) -> Option<usize> {
    let ps = positions(r, lane); if ps.len() >= 256 || ps.iter().any(|p| (p-t).abs() < 0.00001) { return None; }
    let rgba = r.sample(t, smooth); let i = ps.partition_point(|p| *p < t);
    if lane == 0 { r.colors.insert(i, ColorStop { position: t, color: [rgba[0],rgba[1],rgba[2]], midpoint: 0.5 }); }
    else { r.opacities.insert(i, OpacityStop { position: t, opacity: rgba[3], midpoint: 0.5 }); }
    Some(i)
}
pub fn show(ui: &mut egui::Ui, session: &mut Session, id: NodeId) {
    let image = session.current_image(); let Some(node) = session.project.graph.node(id) else { return };
    let display_output = node.choice("output", image) == 0;
    let mut ramp = ramp_of(node, image); let before = ramp.clone();
    let smooth = if node.choice("ramp_interpolation", image) == 1 { node.param("ramp_smoothness", image)/100. } else { 0. };
    let mut state: State = ui.data_mut(|d| d.get_temp(key(id,"state"))).unwrap_or_default();
    ui.label(egui::RichText::new("Gradient ramp").strong());
    ui.label("Click a lane to add a stop; drag a stop to move it.");
    let (resp, painter) = ui.allocate_painter(Vec2::new(ui.available_width().max(100.), 90.), Sense::click_and_drag());
    let rect = Rect::from_min_max(Pos2::new(resp.rect.left()+10., resp.rect.top()+30.), Pos2::new(resp.rect.right()-10., resp.rect.top()+60.));
    ui.data_mut(|d| d.insert_temp(key(id,"rect"), rect));
    for x in 0..100 { let t = x as f32 / 99.; let rgba = ramp.sample(t, smooth); let a = rgba[3]; let bg = if x/5%2 == 0 { 0.65 } else { 0.35 }; let c = Color32::from_rgb(((rgba[0]*a+bg*(1.-a)).clamp(0.,1.)*255.) as u8, ((rgba[1]*a+bg*(1.-a)).clamp(0.,1.)*255.) as u8, ((rgba[2]*a+bg*(1.-a)).clamp(0.,1.)*255.) as u8); let x0 = rect.left()+rect.width()*x as f32/100.; let x1 = rect.left()+rect.width()*(x+1) as f32/100.; painter.rect_filled(Rect::from_min_max(Pos2::new(x0,rect.top()), Pos2::new(x1,rect.bottom())),0.,c); }
    let (pos, pressed, down) = ui.input(|i| (i.pointer.latest_pos(),i.pointer.primary_pressed(),i.pointer.primary_down()));
    if pressed && resp.hovered() { if let Some(p) = pos { let lane = if p.y < rect.center().y { 1 } else { 0 }; let y = if lane == 1 { rect.top()-9. } else { rect.bottom()+9. }; let ps = positions(&ramp,lane); let hit = ps.iter().enumerate().min_by(|(_,a),(_,b)| ((rect.left()+**a*rect.width()-p.x).abs()).total_cmp(&(rect.left()+**b*rect.width()-p.x).abs())).filter(|(_,v)| (rect.left()+**v*rect.width()-p.x).abs() < 9. && (p.y-y).abs() < 13.).map(|(i,_)| i);
        let t = ((p.x-rect.left())/rect.width()).clamp(0.,1.); state.lane=lane; state.selected=hit.or_else(|| add_stop(&mut ramp,lane,t,smooth)); state.dragging=state.selected.is_some();
    }}
    if state.dragging && down { if let (Some(i),Some(p)) = (state.selected,pos) { move_stop(&mut ramp,state.lane,i,((p.x-rect.left())/rect.width()).clamp(0.,1.)); }} else if !down { state.dragging=false; }
    for lane in 0..2 { let y = if lane == 1 { rect.top()-9. } else { rect.bottom()+9. }; for (i,t) in positions(&ramp,lane).iter().enumerate() { let p=Pos2::new(rect.left()+t*rect.width(),y); painter.circle_filled(p,6.,if state.lane==lane && state.selected==Some(i) { Color32::YELLOW } else { Color32::WHITE }); painter.circle_stroke(p,6.,Stroke::new(1.,Color32::BLACK)); }}
    if let Some(i) = state.selected.filter(|i| *i < positions(&ramp,state.lane).len()) {
        ui.label(if state.lane != 0 { "Opacity stop" } else if display_output { "Colour stop (display RGB)" } else { "Colour stop (data RGB)" });
        let mut pos = positions(&ramp,state.lane)[i]; if ui.add(egui::Slider::new(&mut pos,0.0..=1.0).text("Position")).changed() { move_stop(&mut ramp,state.lane,i,pos); }
        if state.lane == 0 { ui.horizontal(|ui| { for (c,label) in ramp.colors[i].color.iter_mut().zip(["R", "G", "B"]) { ui.add(egui::DragValue::new(c).speed(0.01).prefix(label)); } }); } else { ui.add(egui::Slider::new(&mut ramp.opacities[i].opacity,0.0..=1.0).text("Opacity")); }
        let len=positions(&ramp,state.lane).len(); if i+1<len { let m=if state.lane==0 { &mut ramp.colors[i].midpoint } else { &mut ramp.opacities[i].midpoint }; ui.add(egui::Slider::new(m,0.01..=0.99).text("Midpoint to next stop")); }
        if ui.add_enabled(len>2,egui::Button::new("Delete stop")).clicked() { if state.lane==0 { ramp.colors.remove(i); } else { ramp.opacities.remove(i); } state.selected=None; }
    }
    if ramp != before && ramp.validate().is_ok() { if let Some(n)=session.project.graph.node_mut(id) { n.set_data(image,Some(NodeData::GradientRamp(ramp))); } }
    ui.data_mut(|d| d.insert_temp(key(id,"state"),state));
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Modifiers, PointerButton};
    use rawgraph_core::{Mode, NodeKind};
    #[test]
    fn simulated_click_and_drag_edit_both_lanes() {
        let mut s = crate::session::tests::demo_session();
        let id = s.project.graph.add_node(NodeKind::Generator,[0.,0.]);
        let ctx=egui::Context::default(); let mut time=0.;
        let mut frame=|events: Vec<Event>| { let input=egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO,Vec2::new(420.,700.))),time:Some(time),events,..Default::default() }; time+=0.05; ctx.run_ui(input,|ui| { egui::CentralPanel::default().show(ui,|ui| show(ui,&mut s,id)); }).drop_without_applying_deltas(); };
        frame(vec![]); frame(vec![]);
        let rect: Rect=ctx.data(|d| d.get_temp(key(id,"rect"))).unwrap();
        for lane in 0..2 { let y=if lane==0 {rect.bottom()+9.} else {rect.top()-9.}; let a=Pos2::new(rect.left()+rect.width()*0.4,y); let b=Pos2::new(rect.left()+rect.width()*0.7,y);
            frame(vec![Event::PointerMoved(a)]); frame(vec![Event::PointerButton {pos:a,button:PointerButton::Primary,pressed:true,modifiers:Modifiers::NONE}]); frame(vec![Event::PointerMoved(b)]); frame(vec![Event::PointerButton {pos:b,button:PointerButton::Primary,pressed:false,modifiers:Modifiers::NONE}]); frame(vec![]);
        }
        drop(frame);
        let n=s.project.graph.node(id).unwrap(); let r=ramp_of(n,s.current_image()); assert_eq!(r.colors.len(),3); assert_eq!(r.opacities.len(),3); assert!((r.colors[1].position-0.7).abs()<0.01); assert!((r.opacities[1].position-0.7).abs()<0.01); assert!(r.validate().is_ok());
        let image=s.current_image(); let n=s.project.graph.node_mut(id).unwrap(); n.mode=Mode::Individual; let mut individual=r.clone(); individual.colors[1].color=[1.,0.,0.]; assert!(n.set_data(image,Some(NodeData::GradientRamp(individual.clone())))); assert_eq!(ramp_of(n,image),individual); assert_eq!(ramp_of(n,ImageId(image.0+999)),r);
    }
    #[test]
    fn limits_and_order_are_enforced() { let mut r=GradientRamp::default(); for i in 1..255 { assert!(add_stop(&mut r,0,i as f32/255.,0.).is_some()); } assert_eq!(r.colors.len(),256); assert!(add_stop(&mut r,0,0.5,0.).is_none()); move_stop(&mut r,0,1,1.); assert!(r.validate().is_ok()); }
}
