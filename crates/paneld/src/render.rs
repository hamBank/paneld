//! One-shot rendering, shared by `paneld render` and the golden tests.

use anyhow::Context;

use crate::canvas::Canvas;
use crate::config::Config;
use crate::scene::Scene;
use crate::store::StoreSnapshot;
use crate::widgets::FrameCtx;

/// Render a single frame of the named scene.
pub fn render_scene(
    cfg: &Config,
    scene_name: &str,
    ctx: &FrameCtx,
    store: &StoreSnapshot,
) -> anyhow::Result<Canvas> {
    let scene_cfg = cfg.scene(scene_name).with_context(|| {
        let names: Vec<_> = cfg.scenes.iter().map(|s| s.name.as_str()).collect();
        format!("no scene named {scene_name:?} (have: {})", names.join(", "))
    })?;
    let mut scene = Scene::from_config(scene_cfg);
    let mut canvas = Canvas::new(cfg.canvas.width, cfg.canvas.height);
    scene.render(ctx, store, &mut canvas);
    Ok(canvas)
}

/// The card-space framebuffer of the colorlight output at `output` (its
/// index among `[[output]]`) for a rendered canvas: the canvas through the
/// output map, with no gamma, white point or colour order (like the preview).
pub fn card_frame(cfg: &Config, output: usize, canvas: &Canvas) -> anyhow::Result<Canvas> {
    use crate::config::OutputConfig;
    match cfg.outputs.get(output) {
        Some(OutputConfig::Colorlight(cl)) => cl
            .output_map(cfg.canvas.width, cfg.canvas.height)
            .and_then(|m| m.map_canvas(canvas))
            .map_err(anyhow::Error::msg),
        Some(_) => anyhow::bail!("output[{output}] is not a colorlight output"),
        None => anyhow::bail!(
            "there is no output[{output}] ({} output(s) configured)",
            cfg.outputs.len()
        ),
    }
}
