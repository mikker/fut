use std::{
    collections::{HashMap, HashSet},
    env,
    io::{self, Write},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ratatui::layout::Rect;
use uuid::Uuid;

use crate::domain::{KittyImage, KittyPlacement, TerminalId};

use super::{PaneLayoutPolicy, ViewState};

const KITTY_CHUNK_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ImageKey {
    terminal_id: TerminalId,
    image_id: u32,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PlacementKey {
    image: ImageKey,
    identity: PlacementIdentity,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum PlacementIdentity {
    Named(u32),
    Anonymous(u32),
}

#[derive(Clone, Copy, Debug)]
struct HostImage {
    id: u32,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PlacementGeometry {
    host_column: u16,
    host_row: u16,
    columns: u32,
    rows: u32,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
    z: i32,
}

#[derive(Clone, Copy, Debug)]
struct HostPlacement {
    id: u32,
    geometry: PlacementGeometry,
}

#[derive(Clone, Copy, Debug)]
struct RenderedPlacement<'a> {
    key: PlacementKey,
    image: &'a KittyImage,
    geometry: PlacementGeometry,
}

pub(super) struct Renderer {
    supported: bool,
    next_id: u32,
    images: HashMap<ImageKey, HostImage>,
    placements: HashMap<PlacementKey, HostPlacement>,
}

impl Renderer {
    pub(super) fn new() -> Self {
        Self {
            supported: host_supports_kitty(),
            next_id: (Uuid::new_v4().as_u128() as u32).max(1),
            images: HashMap::new(),
            placements: HashMap::new(),
        }
    }

    pub(super) fn sync(
        &mut self,
        writer: &mut impl Write,
        view: &ViewState,
        area: Option<Rect>,
        policy: PaneLayoutPolicy,
        visible: bool,
    ) -> io::Result<()> {
        if !self.supported {
            return Ok(());
        }
        let desired = if visible {
            area.map_or_else(Vec::new, |area| rendered_placements(view, area, policy))
        } else {
            Vec::new()
        };
        let desired_images: HashSet<_> = desired
            .iter()
            .map(|placement| placement.key.image)
            .collect();
        let desired_placements: HashSet<_> =
            desired.iter().map(|placement| placement.key).collect();

        let stale_images: Vec<_> = if visible {
            self.images
                .keys()
                .copied()
                .filter(|key| !desired_images.contains(key))
                .collect()
        } else {
            Vec::new()
        };
        for key in stale_images {
            if let Some(image) = self.images.remove(&key) {
                delete_image(writer, image.id)?;
            }
            self.placements
                .retain(|placement, _| placement.image != key);
        }

        let stale_placements: Vec<_> = self
            .placements
            .iter()
            .filter_map(|(key, placement)| {
                (!desired_placements.contains(key)).then_some((*key, placement.id))
            })
            .collect();
        for (key, placement_id) in stale_placements {
            if let Some(image) = self.images.get(&key.image) {
                delete_placement(writer, image.id, placement_id)?;
            }
            self.placements.remove(&key);
        }

        for placement in desired {
            let image_key = placement.key.image;
            let needs_upload = self
                .images
                .get(&image_key)
                .is_none_or(|host| host.generation != placement.image.generation);
            if needs_upload {
                let host_id = match self.images.get(&image_key) {
                    Some(host) => host.id,
                    None => self.allocate_id(),
                };
                if self.images.contains_key(&image_key) {
                    delete_image(writer, host_id)?;
                    self.placements
                        .retain(|placement, _| placement.image != image_key);
                }
                transmit_png(writer, host_id, &placement.image.png)?;
                self.images.insert(
                    image_key,
                    HostImage {
                        id: host_id,
                        generation: placement.image.generation,
                    },
                );
            }
            let host_image_id = self.images[&image_key].id;
            match self.placements.get(&placement.key).copied() {
                Some(host) if host.geometry == placement.geometry => {}
                Some(host) => {
                    delete_placement(writer, host_image_id, host.id)?;
                    place(writer, host_image_id, host.id, placement.geometry)?;
                    self.placements.insert(
                        placement.key,
                        HostPlacement {
                            id: host.id,
                            geometry: placement.geometry,
                        },
                    );
                }
                None => {
                    let id = self.allocate_id();
                    place(writer, host_image_id, id, placement.geometry)?;
                    self.placements.insert(
                        placement.key,
                        HostPlacement {
                            id,
                            geometry: placement.geometry,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    fn allocate_id(&mut self) -> u32 {
        let id = self.next_id.max(1);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
}

fn host_supports_kitty() -> bool {
    let term = env::var("TERM").unwrap_or_default().to_ascii_lowercase();
    let program = env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    env::var_os("KITTY_WINDOW_ID").is_some()
        || env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
        || env::var_os("WEZTERM_PANE").is_some()
        || matches!(
            program.as_str(),
            "ghostty" | "kitty" | "wezterm" | "warpterminal"
        )
        || term.contains("ghostty")
        || term.contains("kitty")
}

fn rendered_placements(
    view: &ViewState,
    area: Rect,
    policy: PaneLayoutPolicy,
) -> Vec<RenderedPlacement<'_>> {
    let (layouts, _) = view.pane_layouts(area, policy);
    let mut rendered = Vec::new();
    for pane in &view.panes {
        let Some(layout) = layouts.get(&pane.target.terminal_id) else {
            continue;
        };
        let Some(screen) = pane.pending.as_ref() else {
            continue;
        };
        let viewport = Rect::new(
            layout.content.x,
            layout.content.y,
            layout.content.width.min(screen.size.columns),
            layout.content.height.min(screen.size.rows),
        );
        for (ordinal, placement) in screen.graphics.placements.iter().enumerate() {
            let Some(image) = screen
                .graphics
                .images
                .iter()
                .find(|image| image.id == placement.image_id)
            else {
                continue;
            };
            if let Some(clipped) = clip_placement(
                pane.target.terminal_id,
                ordinal as u32,
                placement,
                image,
                viewport,
            ) {
                rendered.push(clipped);
            }
        }
    }
    rendered
}

fn clip_placement<'a>(
    terminal_id: TerminalId,
    ordinal: u32,
    placement: &KittyPlacement,
    image: &'a KittyImage,
    viewport: Rect,
) -> Option<RenderedPlacement<'a>> {
    let left = i64::from(placement.column);
    let top = i64::from(placement.row);
    let right = left + i64::from(placement.columns);
    let bottom = top + i64::from(placement.rows);
    let clipped_left = left.max(0).min(i64::from(viewport.width));
    let clipped_top = top.max(0).min(i64::from(viewport.height));
    let clipped_right = right.max(0).min(i64::from(viewport.width));
    let clipped_bottom = bottom.max(0).min(i64::from(viewport.height));
    if clipped_left >= clipped_right || clipped_top >= clipped_bottom {
        return None;
    }
    let hidden_left = u32::try_from(clipped_left - left).ok()?;
    let hidden_top = u32::try_from(clipped_top - top).ok()?;
    let visible_columns = u32::try_from(clipped_right - clipped_left).ok()?;
    let visible_rows = u32::try_from(clipped_bottom - clipped_top).ok()?;
    let source_right = scaled_edge(
        placement.source_x,
        placement.source_width,
        hidden_left + visible_columns,
        placement.columns,
    );
    let source_bottom = scaled_edge(
        placement.source_y,
        placement.source_height,
        hidden_top + visible_rows,
        placement.rows,
    );
    let source_x = scaled_edge(
        placement.source_x,
        placement.source_width,
        hidden_left,
        placement.columns,
    );
    let source_y = scaled_edge(
        placement.source_y,
        placement.source_height,
        hidden_top,
        placement.rows,
    );
    Some(RenderedPlacement {
        key: PlacementKey {
            image: ImageKey {
                terminal_id,
                image_id: placement.image_id,
            },
            identity: if placement.placement_id == 0 {
                PlacementIdentity::Anonymous(ordinal)
            } else {
                PlacementIdentity::Named(placement.placement_id)
            },
        },
        image,
        geometry: PlacementGeometry {
            host_column: viewport.x.checked_add(u16::try_from(clipped_left).ok()?)?,
            host_row: viewport.y.checked_add(u16::try_from(clipped_top).ok()?)?,
            columns: visible_columns,
            rows: visible_rows,
            source_x,
            source_y,
            source_width: source_right.saturating_sub(source_x),
            source_height: source_bottom.saturating_sub(source_y),
            z: placement.z,
        },
    })
}

fn scaled_edge(origin: u32, length: u32, cells: u32, total_cells: u32) -> u32 {
    origin.saturating_add(
        u32::try_from(u64::from(length) * u64::from(cells) / u64::from(total_cells.max(1)))
            .unwrap_or(u32::MAX),
    )
}

fn transmit_png(writer: &mut impl Write, image_id: u32, png: &[u8]) -> io::Result<()> {
    let encoded = STANDARD.encode(png);
    for (index, chunk) in encoded.as_bytes().chunks(KITTY_CHUNK_BYTES).enumerate() {
        let more = usize::from((index + 1) * KITTY_CHUNK_BYTES < encoded.len());
        if index == 0 {
            write!(writer, "\x1b_Ga=t,f=100,q=2,i={image_id},m={more};")?;
        } else {
            write!(writer, "\x1b_Gq=2,m={more};")?;
        }
        writer.write_all(chunk)?;
        writer.write_all(b"\x1b\\")?;
    }
    Ok(())
}

fn place(
    writer: &mut impl Write,
    image_id: u32,
    placement_id: u32,
    geometry: PlacementGeometry,
) -> io::Result<()> {
    write!(
        writer,
        "\x1b[s\x1b[{};{}H\x1b_Ga=p,q=2,i={},p={},x={},y={},w={},h={},c={},r={},z={},C=1;\x1b\\\x1b[u",
        u32::from(geometry.host_row) + 1,
        u32::from(geometry.host_column) + 1,
        image_id,
        placement_id,
        geometry.source_x,
        geometry.source_y,
        geometry.source_width,
        geometry.source_height,
        geometry.columns,
        geometry.rows,
        geometry.z,
    )
}

fn delete_placement(writer: &mut impl Write, image_id: u32, placement_id: u32) -> io::Result<()> {
    write!(
        writer,
        "\x1b_Ga=d,d=i,q=2,i={image_id},p={placement_id};\x1b\\"
    )
}

fn delete_image(writer: &mut impl Write, image_id: u32) -> io::Result<()> {
    write!(writer, "\x1b_Ga=d,d=I,q=2,i={image_id};\x1b\\")
}

#[cfg(test)]
mod tests {
    use libghostty_vt::{
        Terminal, TerminalOptions,
        kitty::graphics::{self, PlacementIterator},
    };

    use super::*;

    #[test]
    fn clipping_maps_hidden_cells_to_the_exact_source_rectangle() {
        let terminal_id = TerminalId::new();
        let image = KittyImage {
            id: 7,
            generation: 1,
            png: vec![1],
        };
        let placement = KittyPlacement {
            image_id: 7,
            placement_id: 9,
            column: -2,
            row: -1,
            columns: 8,
            rows: 4,
            source_x: 0,
            source_y: 0,
            source_width: 400,
            source_height: 200,
            z: 3,
        };
        let clipped =
            clip_placement(terminal_id, 0, &placement, &image, Rect::new(10, 5, 20, 10)).unwrap();
        assert_eq!(
            (clipped.geometry.host_column, clipped.geometry.host_row),
            (10, 5)
        );
        assert_eq!((clipped.geometry.columns, clipped.geometry.rows), (6, 3));
        assert_eq!(
            (clipped.geometry.source_x, clipped.geometry.source_y),
            (100, 50)
        );
        assert_eq!(
            (
                clipped.geometry.source_width,
                clipped.geometry.source_height
            ),
            (300, 150)
        );
    }

    #[test]
    fn png_transmission_is_chunked_and_placement_uses_host_coordinates() {
        let mut output = Vec::new();
        transmit_png(&mut output, 42, &vec![7; 4_000]).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with("\x1b_Ga=t,f=100,q=2,i=42,m=1;"));
        assert!(text.contains("\x1b_Gq=2,m=0;"));

        let geometry = PlacementGeometry {
            host_column: 4,
            host_row: 2,
            columns: 3,
            rows: 2,
            source_x: 0,
            source_y: 0,
            source_width: 1,
            source_height: 1,
            z: 0,
        };
        let mut output = Vec::new();
        place(&mut output, 42, 11, geometry).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("\x1b[3;5H"));
        assert!(text.contains("a=p,q=2,i=42,p=11"));
        assert!(text.contains("c=3,r=2,z=0,C=1"));
    }

    #[test]
    fn emitted_commands_create_the_translated_image_in_an_outer_terminal() {
        graphics::set_png_decoder(Some(Box::new(graphics::RustPngDecoder::default()))).unwrap();
        let mut outer = Terminal::new(TerminalOptions {
            cols: 20,
            rows: 10,
            max_scrollback: 100,
        })
        .unwrap();
        outer.set_kitty_image_storage_limit(1024 * 1024).unwrap();
        outer.resize(20, 10, 9, 18).unwrap();

        let png = base64::engine::general_purpose::STANDARD
            .decode(
                b"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
            )
            .unwrap();
        let image = KittyImage {
            id: 1,
            generation: 1,
            png,
        };
        let geometry = PlacementGeometry {
            host_column: 4,
            host_row: 2,
            columns: 3,
            rows: 2,
            source_x: 0,
            source_y: 0,
            source_width: 1,
            source_height: 1,
            z: 0,
        };
        let mut output = Vec::new();
        transmit_png(&mut output, 42, &image.png).unwrap();
        place(&mut output, 42, 11, geometry).unwrap();
        outer.vt_write(&output);

        let stored = outer.kitty_graphics().unwrap();
        assert!(stored.image(42).is_some());
        let mut iterator = PlacementIterator::new().unwrap();
        let mut placements = iterator.update(&stored).unwrap();
        let placement = placements.next().unwrap();
        let stored_image = stored.image(42).unwrap();
        let info = placement
            .placement_render_info(&stored_image, &outer)
            .unwrap();
        assert_eq!((info.viewport_col, info.viewport_row), (4, 2));
        assert_eq!((info.grid_cols, info.grid_rows), (3, 2));
    }
}
