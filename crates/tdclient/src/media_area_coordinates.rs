//! Port of `td/telegram/MediaAreaCoordinates.{h,cpp,hpp}` — the shared
//! position/size/rotation/radius record behind the media areas, the
//! story input areas and the TON wallet card stickers. The byte-exact
//! database serialization keeps the C++ `has_radius` flag form
//! (`MediaAreaCoordinates.hpp`); the nested card store plane itself is
//! parked (the session-only card divergence, `ton_wallet.rs`).

use crate::td_api;
use tdmtproto::telegram_api;

/// `telegram_api::mediaAreaCoordinates::RADIUS_MASK` (flags.0).
const RADIUS_MASK: i32 = 1;

/// `td::MediaAreaCoordinates`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MediaAreaCoordinates {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    rotation_angle: f64,
    radius: f64,
}

/// `fix_double` (MediaAreaCoordinates.cpp:16-21) — the non-finite values
/// collapse to zero, the rest clamps to the range.
fn fix_double(value: f64, min_value: f64, max_value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(min_value, max_value)
}

impl MediaAreaCoordinates {
    /// `MediaAreaCoordinates::init` (MediaAreaCoordinates.cpp:22-32) —
    /// the clamped fields; the negative rotation wraps up by 360.
    fn init(
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        rotation_angle: f64,
        radius: f64,
    ) -> MediaAreaCoordinates {
        let mut rotation_angle = fix_double(rotation_angle, -360.0, 360.0);
        if rotation_angle < 0.0 {
            rotation_angle += 360.0;
        }
        MediaAreaCoordinates {
            x: fix_double(x, 0.0, 100.0),
            y: fix_double(y, 0.0, 100.0),
            width: fix_double(width, 0.0, 100.0),
            height: fix_double(height, 0.0, 100.0),
            rotation_angle,
            radius: fix_double(radius, 0.0, 100.0),
        }
    }

    /// `MediaAreaCoordinates(const telegram_api::object_ptr<...>)`
    /// (MediaAreaCoordinates.cpp:34-42) — the null wire object is the
    /// default record.
    pub fn from_telegram(coordinates: Option<&telegram_api::MediaAreaCoordinates>) -> Self {
        let Some(coordinates) = coordinates else {
            return MediaAreaCoordinates::default();
        };
        Self::init(
            coordinates.x,
            coordinates.y,
            coordinates.w,
            coordinates.h,
            coordinates.rotation,
            coordinates.radius,
        )
    }

    /// `MediaAreaCoordinates(const td_api::object_ptr<td_api::areaPosition>)`
    /// (MediaAreaCoordinates.cpp:43-51) — the request-side intake; the
    /// null position is the default record.
    pub fn from_area_position(position: Option<&td_api::AreaPosition>) -> Self {
        let Some(position) = position else {
            return MediaAreaCoordinates::default();
        };
        Self::init(
            position.x_percentage,
            position.y_percentage,
            position.width_percentage,
            position.height_percentage,
            position.rotation_angle,
            position.corner_radius_percentage,
        )
    }

    /// `is_valid` (MediaAreaCoordinates.h:37-39).
    pub fn is_valid(&self) -> bool {
        self.width > 0.0 && self.height > 0.0
    }

    /// `get_area_position_object` (MediaAreaCoordinates.cpp:52-56) — the
    /// caller keeps the C++ `CHECK(is_valid())` shape.
    pub fn get_area_position_object(&self) -> td_api::AreaPosition {
        assert!(self.is_valid());
        td_api::AreaPosition {
            x_percentage: self.x,
            y_percentage: self.y,
            width_percentage: self.width,
            height_percentage: self.height,
            rotation_angle: self.rotation_angle,
            corner_radius_percentage: self.radius,
        }
    }

    /// `get_input_media_area_coordinates` (MediaAreaCoordinates.cpp:57-65)
    /// — the radius rides the flag only when positive.
    pub fn get_input_media_area_coordinates(&self) -> telegram_api::MediaAreaCoordinates {
        assert!(self.is_valid());
        let mut flags = 0;
        if self.radius > 0.0 {
            flags |= RADIUS_MASK;
        }
        telegram_api::MediaAreaCoordinates {
            flags,
            x: self.x,
            y: self.y,
            w: self.width,
            h: self.height,
            rotation: self.rotation_angle,
            radius: self.radius,
        }
    }

    /// `operator==` (MediaAreaCoordinates.cpp:67-73) — the 1e-6 epsilon
    /// per field.
    fn epsilon_eq(&self, rhs: &MediaAreaCoordinates) -> bool {
        const EPSILON: f64 = 1e-6;
        (self.x - rhs.x).abs() < EPSILON
            && (self.y - rhs.y).abs() < EPSILON
            && (self.width - rhs.width).abs() < EPSILON
            && (self.height - rhs.height).abs() < EPSILON
            && (self.rotation_angle - rhs.rotation_angle).abs() < EPSILON
            && (self.radius - rhs.radius).abs() < EPSILON
    }

    /// `MediaAreaCoordinates::store` (MediaAreaCoordinates.hpp:17-35) —
    /// the radius rides its `has_radius` flag.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_radius = self.radius > 0.0;
        crate::store_parse::store_flags(out, [has_radius]);
        crate::store_parse::store_f64(out, self.x);
        crate::store_parse::store_f64(out, self.y);
        crate::store_parse::store_f64(out, self.width);
        crate::store_parse::store_f64(out, self.height);
        crate::store_parse::store_f64(out, self.rotation_angle);
        if has_radius {
            crate::store_parse::store_f64(out, self.radius);
        }
    }

    /// `MediaAreaCoordinates::parse` (MediaAreaCoordinates.hpp:37-58).
    pub fn parse_from(parser: &mut crate::store_parse::Parser) -> Result<Self, &'static str> {
        let [has_radius] = parser.parse_flags::<1>()?;
        let x = parser.f64_()?;
        let y = parser.f64_()?;
        let width = parser.f64_()?;
        let height = parser.f64_()?;
        let rotation_angle = parser.f64_()?;
        let radius = if has_radius { parser.f64_()? } else { 0.0 };
        Result::Ok(Self::init(x, y, width, height, rotation_angle, radius))
    }
}

impl PartialEq for MediaAreaCoordinates {
    fn eq(&self, other: &Self) -> bool {
        self.epsilon_eq(other)
    }
}

#[cfg(test)]
mod tests {
    use super::MediaAreaCoordinates as Coords;
    use crate::td_api;

    /// `init` clamps every field into the C++ ranges.
    #[test]
    fn init_clamps_fields() {
        let coords = Coords::init(120.0, -5.0, 50.0, 30.0, 720.0, 400.0);
        assert_eq!(coords.x, 100.0);
        assert_eq!(coords.y, 0.0);
        assert_eq!(coords.width, 50.0);
        assert_eq!(coords.height, 30.0);
        // 720 clamps to 360 and stays non-negative.
        assert_eq!(coords.rotation_angle, 360.0);
        assert_eq!(coords.radius, 100.0);
    }

    /// The negative rotation wraps up by 360
    /// (MediaAreaCoordinates.cpp:29-31).
    #[test]
    fn negative_rotation_wraps() {
        let coords = Coords::init(0.0, 0.0, 10.0, 10.0, -45.5, 0.0);
        assert!((coords.rotation_angle - 314.5).abs() < 1e-9);
    }

    /// The non-finite values collapse to zero (fix_double).
    #[test]
    fn non_finite_collapses_to_zero() {
        let coords = Coords::init(f64::NAN, f64::INFINITY, 10.0, 10.0, f64::NEG_INFINITY, 5.0);
        assert_eq!(coords.x, 0.0);
        assert_eq!(coords.y, 0.0);
        assert_eq!(coords.rotation_angle, 0.0);
        assert_eq!(coords.radius, 5.0);
        assert!(coords.is_valid());
    }

    /// `is_valid` — both dimensions must be positive.
    #[test]
    fn validity_needs_both_dimensions() {
        assert!(!Coords::init(0.0, 0.0, 0.0, 10.0, 0.0, 0.0).is_valid());
        assert!(!Coords::init(0.0, 0.0, 10.0, -1.0, 0.0, 0.0).is_valid());
        assert!(Coords::init(0.0, 0.0, 1.0, 1.0, 0.0, 0.0).is_valid());
    }

    /// The epsilon equality (operator==).
    #[test]
    fn epsilon_equality() {
        let base = Coords::init(1.0, 2.0, 3.0, 4.0, 5.0, 6.0);
        let same = Coords::init(1.0 + 5e-7, 2.0, 3.0, 4.0, 5.0, 6.0);
        let other = Coords::init(1.0 + 5e-5, 2.0, 3.0, 4.0, 5.0, 6.0);
        assert_eq!(base, same);
        assert_ne!(base, other);
    }

    /// The areaPosition intake and the wire object emission round-trip
    /// the six fields, including the radius flag.
    #[test]
    fn area_position_round_trip() {
        let position = td_api::AreaPosition {
            x_percentage: 10.0,
            y_percentage: 20.0,
            width_percentage: 30.0,
            height_percentage: 40.0,
            rotation_angle: 50.0,
            corner_radius_percentage: 15.0,
        };
        let coords = Coords::from_area_position(Some(&position));
        assert!(coords.is_valid());
        let emitted = coords.get_area_position_object();
        assert_eq!(emitted, position);

        let wire = coords.get_input_media_area_coordinates();
        assert_eq!(wire.flags, 1); // the radius mask
        let re_parsed = Coords::from_telegram(Some(&wire));
        assert_eq!(coords, re_parsed);

        // The radius-less form keeps the flag down.
        let no_radius = Coords::init(10.0, 20.0, 30.0, 40.0, 50.0, 0.0);
        let wire_no_radius = no_radius.get_input_media_area_coordinates();
        assert_eq!(wire_no_radius.flags, 0);
    }

    /// The database serialization round-trips, and the `has_radius` flag
    /// form matches the C++ store (MediaAreaCoordinates.hpp).
    #[test]
    fn store_parse_round_trip() {
        let with_radius = Coords::init(10.0, 20.0, 30.0, 40.0, 50.0, 15.0);
        let mut out = Vec::new();
        with_radius.store(&mut out);
        let parsed = Coords::parse_from(&mut crate::store_parse::Parser::new(&out)).unwrap();
        assert_eq!(with_radius, parsed);

        let without_radius = Coords::init(10.0, 20.0, 30.0, 40.0, 50.0, 0.0);
        let mut out = Vec::new();
        without_radius.store(&mut out);
        let parsed = Coords::parse_from(&mut crate::store_parse::Parser::new(&out)).unwrap();
        assert_eq!(without_radius, parsed);
    }
}
