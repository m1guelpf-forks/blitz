//! A scaled-up image doesn't wrap around at its edges: filtering near an edge of an `<img>`, or of
//! a background image on an axis it doesn't repeat along, blends in that edge's own pixels (as
//! browsers clamp), not the opposite edge's.

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_dom::node::{ImageData, RasterImageData, SpecialElementData, Status};
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::Arc;

const WHITE: [u8; 3] = [255, 255, 255];

/// A 1×2 image: a white pixel over a black one.
fn white_over_black() -> RasterImageData {
    let data = [[255, 255, 255, 255], [0, 0, 0, 255]].concat();
    RasterImageData::new(1, 2, Arc::new(data))
}

/// Renders `html` (20×40 px) with `inject` loading the image, and returns the top row's middle
/// pixel, where the image's white top edge is drawn ten times its size.
fn top_pixel(html: &str, inject: impl FnOnce(&mut HtmlDocument)) -> [u8; 3] {
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            viewport: Some(Viewport::new(20, 40, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    inject(&mut doc);
    doc.resolve(0.0);
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| paint_scene(scene, doc.as_mut(), 1.0, 20, 40, 0, 0),
        20,
        40,
    );
    let index = 5 * 4;
    [buffer[index], buffer[index + 1], buffer[index + 2]]
}

#[test]
fn img_edges_are_clamped() {
    let pixel = top_pixel(
        "<body style='margin:0'><img id=i style='display:block;width:10px;height:20px'></body>",
        |doc| {
            let id = doc.query_selector("#i").unwrap().unwrap();
            let element = doc.get_node_mut(id).unwrap().element_data_mut().unwrap();
            element.special_data =
                SpecialElementData::Image(Box::new(ImageData::Raster(white_over_black())));
        },
    );
    assert_eq!(pixel, WHITE);
}

#[test]
fn background_edges_are_clamped_on_axes_that_dont_repeat() {
    let pixel = top_pixel(
        "<body style='margin:0'><div id=d style='width:10px;height:20px;\
         background:url(https://example.com/x.png) 0 0/10px 20px no-repeat'></div></body>",
        |doc| {
            let id = doc.query_selector("#d").unwrap().unwrap();
            let element = doc.get_node_mut(id).unwrap().element_data_mut().unwrap();
            for layer in element.background_images.iter_mut().flatten() {
                layer.status = Status::Ok;
                layer.image = ImageData::Raster(white_over_black());
            }
        },
    );
    assert_eq!(pixel, WHITE);
}
