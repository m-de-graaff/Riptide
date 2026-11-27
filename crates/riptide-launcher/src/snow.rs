//! The launcher's background: slow pixel snow, drawn on a coarse grid.
//!
//! The simulation knows nothing about the window toolkit, so it is tested on its own;
//! [`Snow::paint`] is the only part that draws.

use eframe::egui::{Color32, Painter, Pos2, Rect, Vec2};

/// The side of one snow pixel, in points.
const PIXEL: f32 = 2.0;
/// Flakes per point of window area.
const DENSITY: f32 = 0.5 / 1100.0;
const MAX_FLAKES: usize = 1500;
/// How far outside the window a flake may go before it wraps or respawns.
const MARGIN: f32 = 16.0;
const WIND: f32 = 9.0;
/// Fall speed and brightness per depth level, far to near.
const LEVEL_FALL: [f32; 4] = [12.0, 18.0, 26.0, 36.0];
const LEVEL_LIGHT: [f32; 4] = [0.22, 0.34, 0.5, 0.68];
/// How likely each level is, far to near.
const LEVEL_WEIGHTS: [f32; 4] = [0.5, 0.28, 0.15, 0.07];
/// Within this distance the pointer brushes flakes aside.
const POINTER_RADIUS: f32 = 80.0;

#[derive(Clone, Debug)]
struct Flake {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    level: usize,
    phase: f32,
    rate: f32,
    sparkle: f32,
}

pub struct Snow {
    flakes: Vec<Flake>,
    seed: u32,
    time: f32,
}

impl Snow {
    pub fn new() -> Self {
        Self {
            flakes: Vec::new(),
            seed: 0x2545_F491,
            time: 0.0,
        }
    }

    /// A number in 0..1 from a small deterministic generator (xorshift).
    fn random(&mut self) -> f32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        (self.seed >> 8) as f32 / (1u32 << 24) as f32
    }

    fn spawn(&mut self, width: f32, height: f32, at_top: bool) -> Flake {
        let pick = self.random();
        let mut total = 0.0;
        let mut level = LEVEL_WEIGHTS.len() - 1;
        for (i, weight) in LEVEL_WEIGHTS.iter().enumerate() {
            total += weight;
            if pick <= total {
                level = i;
                break;
            }
        }
        Flake {
            x: self.random() * width,
            y: if at_top {
                -self.random() * MARGIN
            } else {
                self.random() * height
            },
            vx: 0.0,
            vy: LEVEL_FALL[level],
            level,
            phase: self.random() * std::f32::consts::TAU,
            rate: 0.6 + self.random() * 0.9,
            sparkle: self.random() * 100.0,
        }
    }

    /// Advances the snow by `dt` seconds in a window of `width` by `height` points.
    /// `pointer` is the cursor's position in the window, if it is inside.
    pub fn step(&mut self, dt: f32, width: f32, height: f32, pointer: Option<(f32, f32)>) {
        let dt = dt.clamp(0.0, 0.05);
        self.time += dt;

        let target = ((width * height * DENSITY) as usize).min(MAX_FLAKES);
        while self.flakes.len() < target {
            let flake = self.spawn(width, height, false);
            self.flakes.push(flake);
        }
        self.flakes.truncate(target);

        let ease = 1.0 - (-dt * 2.2).exp();
        for i in 0..self.flakes.len() {
            let time = self.time;
            let flake = &mut self.flakes[i];
            let near = (flake.level + 1) as f32 / LEVEL_FALL.len() as f32;
            let sway = (time * flake.rate + flake.phase).sin() * (3.0 + flake.level as f32 * 3.0);
            flake.vx += (WIND * near + sway - flake.vx) * ease;
            flake.vy += (LEVEL_FALL[flake.level] - flake.vy) * ease;

            if let Some((px, py)) = pointer {
                let (dx, dy) = (flake.x - px, flake.y - py);
                let distance = dx.hypot(dy);
                if distance > 0.001 && distance < POINTER_RADIUS {
                    let closeness = 1.0 - distance / POINTER_RADIUS;
                    let push = closeness * closeness * 900.0 * dt;
                    flake.vx += dx / distance * push;
                    flake.vy += dy / distance * push;
                }
            }

            flake.x += flake.vx * dt;
            flake.y += flake.vy * dt;

            if flake.x < -MARGIN {
                flake.x += width + MARGIN * 2.0;
            } else if flake.x > width + MARGIN {
                flake.x -= width + MARGIN * 2.0;
            }
            if flake.y > height + MARGIN || flake.y < -MARGIN * 2.0 {
                self.flakes[i] = self.spawn(width, height, true);
            }
        }
    }

    /// Draws the flakes into `rect`, snapped to the pixel grid.
    pub fn paint(&self, painter: &Painter, rect: Rect) {
        for flake in &self.flakes {
            let twinkle = (self.time * 0.9 + flake.sparkle).sin().max(0.0).powi(60) * 0.5;
            let light = (LEVEL_LIGHT[flake.level] + twinkle).min(1.0);
            let origin = Pos2::new(
                rect.left() + (flake.x / PIXEL).floor() * PIXEL,
                rect.top() + (flake.y / PIXEL).floor() * PIXEL,
            );
            let cell = |dx: i32, dy: i32, strength: f32| {
                let corner = origin + Vec2::new(dx as f32 * PIXEL, dy as f32 * PIXEL);
                let alpha = (light * strength * 255.0) as u8;
                painter.rect_filled(
                    Rect::from_min_size(corner, Vec2::splat(PIXEL)),
                    0.0,
                    Color32::from_white_alpha(alpha),
                );
            };
            cell(0, 0, 1.0);
            // Nearer flakes are small crystals: a core with arms.
            if flake.level >= 2 {
                for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                    cell(dx, dy, 0.55);
                }
            }
            if flake.level >= 3 {
                for (dx, dy) in [(2, 0), (-2, 0), (0, 2), (0, -2)] {
                    cell(dx, dy, 0.25);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 600.0;
    const H: f32 = 520.0;

    #[test]
    fn the_window_fills_with_flakes_for_its_area() {
        let mut snow = Snow::new();
        snow.step(0.016, W, H, None);
        assert_eq!(snow.flakes.len(), (W * H * DENSITY) as usize);
        snow.step(0.016, W / 2.0, H, None);
        assert_eq!(snow.flakes.len(), (W / 2.0 * H * DENSITY) as usize);
    }

    #[test]
    fn flakes_stay_near_the_window_and_keep_falling() {
        let mut snow = Snow::new();
        for _ in 0..2000 {
            snow.step(0.016, W, H, Some((300.0, 260.0)));
        }
        for flake in &snow.flakes {
            assert!((-MARGIN..=W + MARGIN).contains(&flake.x), "x = {}", flake.x);
            assert!(
                (-MARGIN * 2.0..=H + MARGIN).contains(&flake.y),
                "y = {}",
                flake.y
            );
            assert!(flake.x.is_finite() && flake.y.is_finite());
        }
        let falling = snow.flakes.iter().filter(|f| f.vy > 0.0).count();
        assert!(falling > snow.flakes.len() * 9 / 10);
    }

    #[test]
    fn the_same_steps_give_the_same_snow() {
        let run = || {
            let mut snow = Snow::new();
            for _ in 0..100 {
                snow.step(0.016, W, H, None);
            }
            snow.flakes.iter().map(|f| (f.x, f.y)).collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn a_long_pause_does_not_fling_the_snow() {
        let mut snow = Snow::new();
        snow.step(0.016, W, H, None);
        let before: Vec<f32> = snow.flakes.iter().map(|f| f.y).collect();
        snow.step(30.0, W, H, None);
        for (flake, y) in snow.flakes.iter().zip(before) {
            assert!((flake.y - y).abs() < 5.0 || flake.y < 0.0);
        }
    }
}
