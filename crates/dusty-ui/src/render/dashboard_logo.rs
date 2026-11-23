//! The dashboard's dust logo: "dusty" in pixel letters with a drop shadow, a thin layer of
//! dust on the letter tops, and grains the wind lifts off it. Each cell is an upper half
//! block, so it holds two pixels, one above the other, and the theme colours them.
//! `scripts/make-logo.py` draws the README's sand-coloured version of it.

use std::sync::OnceLock;

use crate::CellGrid;
use crate::LogoPixel;
use crate::Style;

/// Frames in one loop of the blowing dust.
pub const DUST_LOGO_FRAMES: usize = 24;
/// How long each frame shows.
pub const DUST_LOGO_FRAME_MS: u64 = 110;

/// Cells the logo takes: the letters, and room above and to the right for the dust.
pub(super) const DUST_LOGO_WIDTH: u16 = WIDTH as u16;
pub(super) const DUST_LOGO_ROWS: u16 = (HEIGHT / 2) as u16;
/// Rows from the logo's top to the cell row the letters' x-height starts on.
pub(super) const DUST_LOGO_TEXT_ROW: u16 = ((SKY + 3) / 2) as u16;

const WIDTH: usize = 50;
const HEIGHT: usize = 16;
/// Pixel rows of sky above the tallest letter.
const SKY: usize = 4;
const GAP: usize = 2;
const GRAINS: usize = 70;

/// Each letter's first row (counted from the top of `d`) and its rows of pixels.
const LETTERS: [(usize, &[&str]); 5] = [
    (
        0,
        &[
            ".....##", ".....##", ".....##", ".######", "##...##", "##...##", "##...##", "##...##",
            ".######",
        ],
    ),
    (
        3,
        &[
            "##...##", "##...##", "##...##", "##...##", "##...##", ".######",
        ],
    ),
    (
        3,
        &[
            ".######", "##.....", ".#####.", ".....##", ".....##", "######.",
        ],
    ),
    (
        1,
        &[
            ".##...", ".##...", "######", ".##...", ".##...", ".##...", ".##...", "..####",
        ],
    ),
    (
        3,
        &[
            "##...##", "##...##", "##...##", "##...##", ".######", ".....##", ".....##", "######.",
        ],
    ),
];

const INK: LogoPixel = LogoPixel::Letter;
const SHADOW: LogoPixel = LogoPixel::Shadow;
/// Dust from pale to dark.
const DUST: [LogoPixel; 4] = [
    LogoPixel::Dust(0),
    LogoPixel::Dust(1),
    LogoPixel::Dust(2),
    LogoPixel::Dust(3),
];

type Pixels = [[Option<LogoPixel>; WIDTH]; HEIGHT];

struct Grain {
    /// The frame the grain lifts off on.
    start: usize,
    /// Where it is on each frame after that, in pixels; it may leave the logo.
    path: Vec<(isize, isize)>,
}

/// The letters, their shadow and the settled dust, which never move, and every grain's path.
struct Scene {
    still: Pixels,
    grains: Vec<Grain>,
}

/// A small fixed-seed generator, so the dust blows the same way every run.
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        self.next() as usize % bound
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

impl Scene {
    fn new() -> Self {
        let mut ink = [[false; WIDTH]; HEIGHT];
        let mut left = 0;
        for (top, rows) in LETTERS {
            for (y, row) in rows.iter().enumerate() {
                for (x, pixel) in row.bytes().enumerate() {
                    ink[SKY + top + y][left + x] = pixel == b'#';
                }
            }
            left += rows.iter().map(|row| row.len()).max().unwrap_or(0) + GAP;
        }

        let mut still = [[None; WIDTH]; HEIGHT];
        for (y, row) in ink.iter().enumerate().take(HEIGHT - 1) {
            for (x, &set) in row.iter().enumerate().take(WIDTH - 1) {
                if set {
                    still[y + 1][x + 1] = Some(SHADOW);
                }
            }
        }
        for (still_row, ink_row) in still.iter_mut().zip(&ink) {
            for (pixel, &set) in still_row.iter_mut().zip(ink_row) {
                if set {
                    *pixel = Some(INK);
                }
            }
        }

        // Dust settles one or two pixels deep on each column's top; grains lift off the
        // outer tops only, not out of the cups of u and y.
        let mut rng = Rng(0x9e37_79b9);
        let mut sources = Vec::new();
        for x in 0..WIDTH {
            let Some(top) = ink.iter().position(|row| row[x]) else {
                continue;
            };
            let depth = if rng.chance(25) { 2 } else { 1 };
            for k in 1..=depth {
                still[top - k][x] = Some(if k == 1 && rng.chance(50) {
                    DUST[3]
                } else {
                    DUST[2]
                });
            }
            if top <= SKY + 4 {
                sources.push((x as isize, (top - depth) as isize));
            }
        }

        let grains = (0..GRAINS)
            .map(|_| {
                let (x, y) = sources[rng.below(sources.len())];
                let life = 6 + rng.below(7);
                let start = rng.below(DUST_LOGO_FRAMES);
                let mut path = vec![(x, y)];
                for step in 0..life - 1 {
                    let (x, y) = path[path.len() - 1];
                    let right = isize::from(rng.chance(75));
                    let up = isize::from(rng.chance(if step < 3 { 70 } else { 40 }));
                    path.push((x + right, y - up));
                }
                Grain { start, path }
            })
            .collect();
        Self { still, grains }
    }

    /// The picture on `frame`: grains darkest as they lift and paler as they rise, passing
    /// behind the letters and the settled dust.
    fn pixels(&self, frame: usize) -> Pixels {
        let mut pixels = self.still;
        let frame = frame % DUST_LOGO_FRAMES;
        for grain in &self.grains {
            let age = (frame + DUST_LOGO_FRAMES - grain.start) % DUST_LOGO_FRAMES;
            let Some(&(x, y)) = grain.path.get(age) else {
                continue;
            };
            let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
                continue;
            };
            if x < WIDTH && y < HEIGHT && pixels[y][x].is_none() {
                let tone = 3 - (age * 4 / grain.path.len()).min(3);
                pixels[y][x] = Some(DUST[tone]);
            }
        }
        pixels
    }
}

fn scene() -> &'static Scene {
    static SCENE: OnceLock<Scene> = OnceLock::new();
    SCENE.get_or_init(Scene::new)
}

/// Draw `frame` of the logo with its top-left cell at `(x, y)`, clipped before `right`.
/// Cells with neither pixel set are left as they are.
pub(super) fn draw_dust_logo(grid: &mut CellGrid, x: u16, y: u16, right: u16, frame: usize) {
    let pixels = scene().pixels(frame);
    for (row, pair) in (0..).zip(pixels.chunks(2)) {
        for (column, (&upper, &lower)) in (0..).zip(pair[0].iter().zip(&pair[1])) {
            if x + column >= right {
                break;
            }
            if upper.is_some() || lower.is_some() {
                grid.set_cell(x + column, y + row, "▀", Style::Pixels(upper, lower));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_letters_and_their_shadow_stay_put_while_the_dust_moves() {
        let first = scene().pixels(0);
        let later = scene().pixels(5);
        assert_ne!(first, later, "the dust should move between frames");
        for frame in 0..DUST_LOGO_FRAMES {
            let pixels = scene().pixels(frame);
            for (y, (before, now)) in first.iter().zip(&pixels).enumerate() {
                for (x, (before, now)) in before.iter().zip(now).enumerate() {
                    for color in [INK, SHADOW] {
                        assert_eq!(
                            *before == Some(color),
                            *now == Some(color),
                            "({x}, {y}) on frame {frame}"
                        );
                    }
                }
            }
        }
        // The loop closes: the frame after the last is the first again.
        assert_eq!(scene().pixels(DUST_LOGO_FRAMES), first);
    }

    #[test]
    fn the_logo_spells_dusty_in_its_rows() {
        let pixels = scene().pixels(0);
        // The d's stem starts at the top of the letters, and y's tail reaches the bottom row
        // above the shadow.
        assert_eq!(pixels[SKY][5], Some(INK));
        assert_eq!(pixels[SKY + 10][35 + 1], Some(INK));
        assert_eq!(pixels[SKY + 11][35 + 2], Some(SHADOW));
        assert_eq!(usize::from(DUST_LOGO_ROWS) * 2, HEIGHT);
    }
}
