use ratatui::style::Color;

// Activity colors (cyan spectrum)
pub const ACTIVE_BRIGHT: Color = Color::Rgb(0, 255, 220);
pub const ACTIVE_MID: Color = Color::Rgb(0, 180, 160);
pub const ACTIVE_DIM: Color = Color::Rgb(0, 80, 70);
pub const INACTIVE: Color = Color::Rgb(35, 42, 42);

// Error severity gradient (foreground)
pub const ERR_NONE: Color = Color::Rgb(60, 180, 100);
pub const ERR_LOW: Color = Color::Rgb(220, 200, 50);
pub const ERR_MED: Color = Color::Rgb(240, 130, 40);
pub const ERR_HIGH: Color = Color::Rgb(255, 50, 30);
pub const ERR_FIRE: Color = Color::Rgb(255, 0, 80);

// Error severity gradient (background -- subtle tints for dark terminals)
pub const ERR_BG_MIN: Color = Color::Rgb(45, 40, 20);
pub const ERR_BG_LOW: Color = Color::Rgb(50, 38, 18);
pub const ERR_BG_MED: Color = Color::Rgb(55, 30, 15);
pub const ERR_BG_HIGH: Color = Color::Rgb(60, 20, 15);
pub const ERR_BG_FIRE: Color = Color::Rgb(70, 12, 18);

pub const HEADER_CYAN: Color = Color::Rgb(80, 200, 255);
pub const DIM: Color = Color::Rgb(100, 100, 110);
pub const TEXT: Color = Color::Rgb(200, 200, 210);
pub const TEXT_BRIGHT: Color = Color::Rgb(240, 240, 250);
pub const SEPARATOR: Color = Color::Rgb(60, 65, 70);

pub const PROGRESS_PAUSED: Color = Color::Rgb(200, 180, 50);

pub const LOG_ERROR: Color = Color::Rgb(255, 80, 80);
pub const LOG_WARN: Color = Color::Rgb(240, 200, 60);
pub const LOG_INFO: Color = Color::Rgb(80, 200, 140);
pub const LOG_DEBUG: Color = Color::Rgb(100, 150, 255);
pub const LOG_TRACE: Color = Color::Rgb(80, 80, 90);

#[must_use]
pub fn lerp(a: Color, b: Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let r = (f64::from(r1) + (f64::from(r2) - f64::from(r1)) * t) as u8;
            let g = (f64::from(g1) + (f64::from(g2) - f64::from(g1)) * t) as u8;
            let b = (f64::from(b1) + (f64::from(b2) - f64::from(b1)) * t) as u8;
            Color::Rgb(r, g, b)
        }
        _ => {
            if t < 0.5 {
                a
            } else {
                b
            }
        }
    }
}

#[must_use]
pub fn error_severity(error_count: usize) -> Color {
    match error_count {
        0 => ERR_NONE,
        1 => ERR_LOW,
        2..=5 => lerp(ERR_LOW, ERR_MED, (error_count as f64 - 2.0) / 3.0),
        6..=20 => lerp(ERR_MED, ERR_HIGH, (error_count as f64 - 6.0) / 14.0),
        _ => lerp(
            ERR_HIGH,
            ERR_FIRE,
            ((error_count as f64 - 20.0) / 30.0).min(1.0),
        ),
    }
}

/// Background color for error cells. Fades with age but has a warm-yellow floor.
#[must_use]
pub fn error_bg(error_count: usize, age_secs: f64) -> Option<Color> {
    if error_count == 0 {
        return None;
    }
    let peak = match error_count {
        1 => ERR_BG_LOW,
        2..=5 => lerp(ERR_BG_LOW, ERR_BG_MED, (error_count as f64 - 2.0) / 3.0),
        6..=20 => lerp(ERR_BG_MED, ERR_BG_HIGH, (error_count as f64 - 6.0) / 14.0),
        _ => lerp(
            ERR_BG_HIGH,
            ERR_BG_FIRE,
            ((error_count as f64 - 20.0) / 30.0).min(1.0),
        ),
    };
    let fade = (age_secs / 10.0).min(1.0);
    Some(lerp(peak, ERR_BG_MIN, fade))
}

#[must_use]
pub fn activity_color(brightness: f64) -> Color {
    if brightness > 0.7 {
        lerp(ACTIVE_MID, ACTIVE_BRIGHT, (brightness - 0.7) / 0.3)
    } else if brightness > 0.2 {
        lerp(ACTIVE_DIM, ACTIVE_MID, (brightness - 0.2) / 0.5)
    } else if brightness > 0.0 {
        lerp(INACTIVE, ACTIVE_DIM, brightness / 0.2)
    } else {
        INACTIVE
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn rgb(c: Color) -> (u8, u8, u8) {
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => panic!("expected Rgb color"),
        }
    }

    #[test]
    fn lerp_at_zero_returns_first_color() {
        let result = lerp(Color::Rgb(100, 0, 0), Color::Rgb(0, 100, 0), 0.0);
        check!(rgb(result) == (100, 0, 0));
    }

    #[test]
    fn lerp_at_one_returns_second_color() {
        let result = lerp(Color::Rgb(100, 0, 0), Color::Rgb(0, 100, 0), 1.0);
        check!(rgb(result) == (0, 100, 0));
    }

    #[test]
    fn lerp_at_half_midpoint() {
        let result = lerp(Color::Rgb(0, 0, 0), Color::Rgb(100, 100, 100), 0.5);
        check!(rgb(result) == (50, 50, 50));
    }

    #[test]
    fn lerp_clamps_above_one() {
        let result = lerp(Color::Rgb(0, 0, 0), Color::Rgb(200, 200, 200), 2.0);
        check!(rgb(result) == (200, 200, 200));
    }

    #[test]
    fn lerp_clamps_below_zero() {
        let result = lerp(Color::Rgb(0, 0, 0), Color::Rgb(200, 200, 200), -1.0);
        check!(rgb(result) == (0, 0, 0));
    }

    #[test]
    fn lerp_non_rgb_below_half_returns_first() {
        let result = lerp(Color::Red, Color::Blue, 0.3);
        check!(result == Color::Red);
    }

    #[test]
    fn lerp_non_rgb_above_half_returns_second() {
        let result = lerp(Color::Red, Color::Blue, 0.7);
        check!(result == Color::Blue);
    }

    #[test]
    fn lerp_mixed_rgb_and_named_below_half() {
        let result = lerp(Color::Rgb(10, 20, 30), Color::Green, 0.2);
        // Falls through to non-RGB branch since b is not Rgb
        check!(result == Color::Rgb(10, 20, 30));
    }

    #[test]
    fn error_severity_zero_is_green() {
        check!(error_severity(0) == ERR_NONE);
    }

    #[test]
    fn error_severity_one_is_low() {
        check!(error_severity(1) == ERR_LOW);
    }

    #[test]
    fn error_severity_mid_range() {
        let c = error_severity(3);
        // Should be between ERR_LOW and ERR_MED
        let (r, _, _) = rgb(c);
        let (r_low, _, _) = rgb(ERR_LOW);
        let (r_med, _, _) = rgb(ERR_MED);
        assert!(r >= r_low.min(r_med) && r <= r_low.max(r_med));
    }

    #[test]
    fn error_severity_high_range() {
        let c = error_severity(10);
        let (r, _, _) = rgb(c);
        // Should be in the ERR_MED to ERR_HIGH range
        assert!(
            r > 200,
            "high error count should have red component > 200, got {r}"
        );
    }

    #[test]
    fn error_severity_extreme() {
        let c = error_severity(100);
        let (r, _, _) = rgb(c);
        // Should be near ERR_FIRE
        assert!(r == 255, "extreme error count should have r=255, got {r}");
    }

    #[test]
    fn error_bg_zero_returns_none() {
        assert!(error_bg(0, 0.0).is_none());
    }

    #[test]
    fn error_bg_nonzero_returns_some() {
        assert!(error_bg(1, 0.0).is_some());
    }

    #[test]
    fn error_bg_fades_with_age() {
        let fresh = error_bg(5, 0.0).unwrap();
        let aged = error_bg(5, 10.0).unwrap();
        // Aged should be closer to ERR_BG_MIN
        let (r_fresh, _, _) = rgb(fresh);
        let (r_aged, _, _) = rgb(aged);
        let (r_min, _, _) = rgb(ERR_BG_MIN);
        // Aged r should be closer to min than fresh r
        assert!(
            (i16::from(r_aged) - i16::from(r_min)).unsigned_abs()
                <= (i16::from(r_fresh) - i16::from(r_min)).unsigned_abs(),
            "aged bg should be closer to ERR_BG_MIN"
        );
    }

    #[test]
    fn error_bg_high_count() {
        let bg = error_bg(50, 0.0).unwrap();
        let (r, _, _) = rgb(bg);
        // Should be near ERR_BG_FIRE range
        assert!(r >= 60, "high error bg should have r >= 60, got {r}");
    }

    #[test]
    fn activity_color_zero_is_inactive() {
        check!(activity_color(0.0) == INACTIVE);
    }

    #[test]
    fn activity_color_negative_is_inactive() {
        // lerp clamps, so -0.1 effectively maps to brightness <= 0.0
        check!(activity_color(-0.5) == INACTIVE);
    }

    #[test]
    fn activity_color_full_brightness() {
        let c = activity_color(1.0);
        // Should be near ACTIVE_BRIGHT
        check!(c == ACTIVE_BRIGHT);
    }

    #[test]
    fn activity_color_mid_brightness() {
        let c = activity_color(0.5);
        // Should be in the ACTIVE_DIM to ACTIVE_MID range
        let (_, g, _) = rgb(c);
        assert!(
            g > 80 && g < 200,
            "mid brightness green should be moderate, got {g}"
        );
    }

    #[test]
    fn activity_color_low_brightness() {
        let c = activity_color(0.1);
        // Should be between INACTIVE and ACTIVE_DIM
        let (_, g, _) = rgb(c);
        let (_, g_inactive, _) = rgb(INACTIVE);
        let (_, g_dim, _) = rgb(ACTIVE_DIM);
        assert!(g >= g_inactive && g <= g_dim);
    }
}
