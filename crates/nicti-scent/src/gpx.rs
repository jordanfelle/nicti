//! GPS track logs in GPX 1.0 / 1.1 (<https://www.topografix.com/gpx.asp>) for geotagging at import
//! (#431): the timed track points of every `<trk>`, and the position at any moment.
//!
//! What we use from the format: `<trk>` -> `<trkseg>` -> `<trkpt lat=.. lon=..>` with optional `<ele>`
//! (metres) and `<time>` (an `xsd:dateTime`, UTC per the schema; a time without a zone is read as
//! UTC). Points without a time can't be matched to a photo and are left out; waypoints (`<wpt>`) and
//! routes (`<rte>`) carry no timeline and are ignored. A track segment is one continuous span of
//! logging, so positions are only interpolated *within* a segment, never across a reception gap.
//!
//! Camera clocks: EXIF `DateTimeOriginal` has no zone, so [`camera_time_to_unix`] takes the offset
//! of the camera's clock from UTC (plus a drift correction) explicitly; the caller decides the
//! policy rather than this module guessing the local zone.

use quick_xml::events::Event;

/// A position, as written to the XMP `exif:GPS*` properties.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gps {
    pub latitude: f64,
    pub longitude: f64,
    /// Metres above sea level.
    pub altitude: Option<f64>,
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn num(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `YYYY<sep>MM<sep>DD[T| ]hh:mm:ss[.fff]` -> (seconds since epoch ignoring any zone, zone offset
/// in minutes if the text carried one). `sep` is `-` (ISO) or `:` (EXIF).
fn parse_civil(s: &str) -> Option<(f64, Option<i64>)> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || !(b[4] == b'-' || b[4] == b':') || b[7] != b[4] {
        return None;
    }
    if !(b[10] == b'T' || b[10] == b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(&s[0..4])?, num(&s[5..7])?, num(&s[8..10])?);
    let (h, mi, sec) = (num(&s[11..13])?, num(&s[14..16])?, num(&s[17..19])?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut frac = 0.0;
    if let Some(r) = rest.strip_prefix('.') {
        let n = r.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        frac = format!("0.{}", &r[..n]).parse().ok()?;
        rest = &r[n..];
    }
    let zone = match rest {
        "" => None,
        "Z" | "z" => Some(0),
        z if z.len() == 6
            && (z.starts_with('+') || z.starts_with('-'))
            && z.as_bytes()[3] == b':' =>
        {
            let m = num(&z[1..3])? * 60 + num(&z[4..6])?;
            Some(if z.starts_with('-') { -m } else { m })
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    Some((
        (days * 86_400 + h * 3600 + mi * 60 + sec) as f64 + frac,
        zone,
    ))
}

/// An `xsd:dateTime` as seconds since the Unix epoch; no zone = UTC (the GPX schema's convention).
/// A bare date is too coarse to place a photo and is rejected.
fn parse_time(s: &str) -> Option<f64> {
    let (secs, zone) = parse_civil(s)?;
    Some(secs - (zone.unwrap_or(0) * 60) as f64)
}

/// An EXIF `DateTimeOriginal` (`2026:05:01 10:00:30`, optionally with sub-seconds or a zone) as
/// seconds since the Unix epoch. The text's own zone wins; otherwise the camera clock is taken to
/// be `camera_utc_offset_minutes` ahead of UTC. `drift_seconds` is how far the camera clock runs
/// ahead of true time (subtracted), for a clock that was never set right.
pub fn camera_time_to_unix(
    exif: &str,
    camera_utc_offset_minutes: i32,
    drift_seconds: f64,
) -> Option<f64> {
    let (secs, zone) = parse_civil(exif)?;
    let off = zone.unwrap_or(i64::from(camera_utc_offset_minutes));
    Some(secs - (off * 60) as f64 - drift_seconds)
}

/// Nesting / size limits (a track log is flat; these only stop hostile input).
const MAX_DEPTH: usize = 64;
const MAX_POINTS: usize = 5_000_000;

/// One timed point of a track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackPoint {
    /// Seconds since 1970-01-01T00:00:00Z (fractional when the log has sub-second times).
    pub time: f64,
    pub latitude: f64,
    pub longitude: f64,
    /// Metres above sea level.
    pub elevation: Option<f64>,
    /// Track segment number, counted across the whole file (continuous logging spans).
    pub segment: u32,
}

/// How a position was found by [`Tracklog::locate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    /// The time falls between two points of the same segment: linear interpolation.
    Interpolated,
    /// The nearest point (at a segment's start or end, or across a gap).
    Nearest,
}

/// The timed points of a GPX file, sorted by time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tracklog {
    pub points: Vec<TrackPoint>,
    /// Tracks (`<trk>`) in the file.
    pub tracks: usize,
    /// Track points without a usable time (left out of `points`).
    pub untimed: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GpxError {
    Xml(String),
    /// Not a GPX document (no `<gpx>` root).
    NotGpx,
    /// Nesting / size limits exceeded.
    TooComplex,
}

impl std::fmt::Display for GpxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpxError::Xml(e) => write!(f, "malformed GPX: {e}"),
            GpxError::NotGpx => f.write_str("not a GPX file (no <gpx> element)"),
            GpxError::TooComplex => f.write_str("GPX file too large or too deeply nested"),
        }
    }
}

impl std::error::Error for GpxError {}

/// Local name of a possibly prefixed XML name (`gpx:trkpt` → `trkpt`).
fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

#[derive(Default)]
struct Pending {
    lat: Option<f64>,
    lon: Option<f64>,
    ele: Option<f64>,
    time: Option<f64>,
}

/// Parse a GPX document.
pub fn parse_gpx(s: &str) -> Result<Tracklog, GpxError> {
    let mut reader = quick_xml::Reader::from_str(s);
    reader.config_mut().check_end_names = false;
    let mut path: Vec<String> = Vec::new();
    let mut seen_gpx = false;
    let mut log = Tracklog::default();
    let mut segment: u32 = 0;
    let mut pt: Option<Pending> = None;
    let mut text = String::new();
    loop {
        let ev = reader
            .read_event()
            .map_err(|e| GpxError::Xml(e.to_string()))?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let name = local(e.name().as_ref()).to_owned();
                let parent = path.last().map(String::as_str);
                match name.as_str() {
                    "gpx" if path.is_empty() => seen_gpx = true,
                    "trk" if parent == Some("gpx") => log.tracks += 1,
                    "trkseg" if parent == Some("trk") => {
                        if log.points.last().is_some_and(|p| p.segment == segment) {
                            segment += 1;
                        }
                    }
                    "trkpt" if parent == Some("trkseg") => {
                        let mut p = Pending::default();
                        for a in e.attributes().with_checks(false).flatten() {
                            let v = &a.value;
                            match local(a.key.as_ref()) {
                                "lat" => p.lat = v.trim().parse().ok(),
                                "lon" => p.lon = v.trim().parse().ok(),
                                _ => {}
                            }
                        }
                        pt = Some(p);
                    }
                    _ => {}
                }
                text.clear();
                if matches!(ev, Event::Start(_)) {
                    if path.len() >= MAX_DEPTH {
                        return Err(GpxError::TooComplex);
                    }
                    path.push(name);
                } else if name == "trkpt" {
                    finish_point(&mut log, pt.take(), segment)?;
                }
            }
            Event::Text(t) => text.push_str(&t.xml_content(quick_xml::XmlVersion::Implicit1_0)),
            Event::CData(t) => text.push_str(&t.into_inner()),
            Event::End(_) => {
                let Some(name) = path.pop() else { continue };
                let parent = path.last().map(String::as_str);
                match name.as_str() {
                    "ele" if parent == Some("trkpt") => {
                        if let Some(p) = pt.as_mut() {
                            p.ele = text.trim().parse().ok();
                        }
                    }
                    "time" if parent == Some("trkpt") => {
                        if let Some(p) = pt.as_mut() {
                            p.time = parse_time(text.trim());
                        }
                    }
                    "trkpt" => finish_point(&mut log, pt.take(), segment)?,
                    _ => {}
                }
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !seen_gpx {
        return Err(GpxError::NotGpx);
    }
    // stable: points logged at the same second keep their file order
    log.points.sort_by(|a, b| a.time.total_cmp(&b.time));
    Ok(log)
}

fn finish_point(log: &mut Tracklog, p: Option<Pending>, segment: u32) -> Result<(), GpxError> {
    let Some(p) = p else { return Ok(()) };
    let (Some(lat), Some(lon)) = (p.lat, p.lon) else {
        return Ok(());
    };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Ok(());
    }
    let Some(time) = p.time else {
        log.untimed += 1;
        return Ok(());
    };
    if log.points.len() >= MAX_POINTS {
        return Err(GpxError::TooComplex);
    }
    log.points.push(TrackPoint {
        time,
        latitude: lat,
        longitude: lon,
        elevation: p.ele,
        segment,
    });
    Ok(())
}

impl Tracklog {
    /// First and last point times (seconds since the epoch, UTC).
    pub fn span(&self) -> Option<(f64, f64)> {
        Some((self.points.first()?.time, self.points.last()?.time))
    }

    /// Where the logger was at `time` (seconds since the epoch, UTC).
    ///
    /// Between two points of the same segment that are at most `max_gap` seconds apart, the position
    /// is interpolated linearly in time. Otherwise the nearest point counts if it is within `max_gap`
    /// seconds; beyond that there is no position (the photo was taken while nothing was logged).
    pub fn locate(&self, time: f64, max_gap: f64) -> Option<(Gps, Match)> {
        let pts = &self.points;
        if pts.is_empty() || !time.is_finite() {
            return None;
        }
        let i = pts.partition_point(|p| p.time < time);
        let next = pts.get(i);
        let prev = i.checked_sub(1).and_then(|j| pts.get(j));
        if let Some(n) = next.filter(|n| n.time == time) {
            return Some((gps(n), Match::Interpolated));
        }
        if let (Some(a), Some(b)) = (prev, next) {
            if a.segment == b.segment && b.time - a.time <= max_gap {
                let t = (time - a.time) / (b.time - a.time);
                return Some((lerp(a, b, t), Match::Interpolated));
            }
        }
        let near = match (prev, next) {
            (Some(a), Some(b)) => {
                if time - a.time <= b.time - time {
                    a
                } else {
                    b
                }
            }
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => return None,
        };
        ((near.time - time).abs() <= max_gap).then(|| (gps(near), Match::Nearest))
    }
}

/// Camera-clock policy and matching limit for [`geotag`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeotagOptions {
    /// Offset of the camera's clock from UTC, in minutes (EXIF times carry no zone).
    pub camera_utc_offset_minutes: i32,
    /// Seconds the camera clock runs ahead of true time.
    pub drift_seconds: f64,
    /// Longest gap between track points still bridged / the nearest-point tolerance.
    pub max_gap_seconds: f64,
}

impl Default for GeotagOptions {
    fn default() -> Self {
        Self {
            camera_utc_offset_minutes: 0,
            drift_seconds: 0.0,
            max_gap_seconds: 300.0,
        }
    }
}

/// Why [`geotag`] changed nothing or failed.
#[derive(Debug, thiserror::Error)]
pub enum GeotagError {
    #[error("capture time is missing or unreadable")]
    NoCaptureTime,
    #[error("no track position within the matching limit of the capture time")]
    NoPosition,
    #[error(transparent)]
    Patch(#[from] crate::packet::PatchError),
}

/// Looks up where the photo taken at `captured_at` (EXIF `DateTimeOriginal` text) was, and writes
/// the position into `xmp`, preserving everything else in the packet.
pub fn geotag(
    xmp: &str,
    log: &Tracklog,
    captured_at: &str,
    opts: &GeotagOptions,
) -> Result<(String, Match), GeotagError> {
    let t = camera_time_to_unix(
        captured_at,
        opts.camera_utc_offset_minutes,
        opts.drift_seconds,
    )
    .ok_or(GeotagError::NoCaptureTime)?;
    let (pos, how) = log
        .locate(t, opts.max_gap_seconds)
        .ok_or(GeotagError::NoPosition)?;
    let patch = crate::packet::Patch {
        gps: Some(Some(pos)),
        ..Default::default()
    };
    Ok((crate::packet::apply(xmp, &patch)?, how))
}

fn gps(p: &TrackPoint) -> Gps {
    Gps {
        latitude: p.latitude,
        longitude: p.longitude,
        altitude: p.elevation,
    }
}

/// Linear interpolation, taking the short way across the ±180° meridian.
fn lerp(a: &TrackPoint, b: &TrackPoint, t: f64) -> Gps {
    let mut dlon = b.longitude - a.longitude;
    if dlon > 180.0 {
        dlon -= 360.0;
    } else if dlon < -180.0 {
        dlon += 360.0;
    }
    let mut lon = a.longitude + dlon * t;
    if lon > 180.0 {
        lon -= 360.0;
    } else if lon < -180.0 {
        lon += 360.0;
    }
    let altitude = match (a.elevation, b.elevation) {
        (Some(x), Some(y)) => Some(x + (y - x) * t),
        (x, y) => x.or(y),
    };
    Gps {
        latitude: a.latitude + (b.latitude - a.latitude) * t,
        longitude: lon,
        altitude,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<gpx version="1.1" creator="test" xmlns="http://www.topografix.com/GPX/1/1">
  <wpt lat="1.0" lon="1.0"><time>2026-05-01T10:00:30Z</time></wpt>
  <trk><name>Walk</name>
    <trkseg>
      <trkpt lat="46.0000" lon="7.0000"><ele>1600</ele><time>2026-05-01T10:00:00Z</time></trkpt>
      <trkpt lat="46.0010" lon="7.0020"><ele>1610</ele><time>2026-05-01T10:01:00Z</time></trkpt>
      <trkpt lat="46.0020" lon="7.0040"><time>2026-05-01T10:02:00.500Z</time></trkpt>
      <trkpt lat="46.5" lon="7.5"/>
    </trkseg>
    <trkseg>
      <trkpt lat="47.0000" lon="8.0000"><time>2026-05-01T12:00:00Z</time></trkpt>
      <trkpt lat="47.0010" lon="8.0010"><time>2026-05-01T12:00:10+00:00</time></trkpt>
    </trkseg>
  </trk>
</gpx>"#;

    fn t(s: &str) -> f64 {
        parse_time(s).unwrap()
    }

    #[test]
    fn parses_timed_track_points() {
        let log = parse_gpx(LOG).unwrap();
        assert_eq!(log.tracks, 1);
        assert_eq!(
            log.points.len(),
            5,
            "waypoints and the untimed point are left out"
        );
        assert_eq!(log.untimed, 1);
        assert_eq!(log.points[0].elevation, Some(1600.0));
        assert_eq!(log.points[2].time, t("2026-05-01T10:02:00Z") + 0.5);
        assert_eq!(
            (
                log.points[0].segment,
                log.points[2].segment,
                log.points[3].segment
            ),
            (0, 0, 1)
        );
        assert_eq!(
            log.span(),
            Some((t("2026-05-01T10:00:00Z"), t("2026-05-01T12:00:10Z")))
        );
        // 2026-05-01T10:00:00Z
        assert_eq!(log.points[0].time, 1_777_629_600.0);
    }

    #[test]
    fn interpolates_within_a_segment() {
        let log = parse_gpx(LOG).unwrap();
        let (g, m) = log.locate(t("2026-05-01T10:00:30Z"), 300.0).unwrap();
        assert_eq!(m, Match::Interpolated);
        assert!(
            (g.latitude - 46.0005).abs() < 1e-9 && (g.longitude - 7.0010).abs() < 1e-9,
            "{g:?}"
        );
        assert_eq!(g.altitude, Some(1605.0));
        // exactly on a point
        let (g, _) = log.locate(t("2026-05-01T10:01:00Z"), 300.0).unwrap();
        assert_eq!((g.latitude, g.longitude), (46.0010, 7.0020));
    }

    #[test]
    fn never_interpolates_across_segments() {
        let log = parse_gpx(LOG).unwrap();
        // between the segments (10:02 … 12:00): only near either end
        let (g, m) = log.locate(t("2026-05-01T10:04:00Z"), 300.0).unwrap();
        assert_eq!((m, g.latitude), (Match::Nearest, 46.0020));
        let (g, m) = log.locate(t("2026-05-01T11:58:00Z"), 300.0).unwrap();
        assert_eq!((m, g.latitude), (Match::Nearest, 47.0));
        assert!(
            log.locate(t("2026-05-01T11:00:00Z"), 300.0).is_none(),
            "an hour from any point"
        );
        // before the first and after the last point
        assert!(log.locate(t("2026-05-01T09:59:00Z"), 300.0).is_some());
        assert!(log.locate(t("2026-05-01T09:50:00Z"), 300.0).is_none());
        assert!(log.locate(t("2026-05-01T12:10:00Z"), 300.0).is_none());
    }

    #[test]
    fn max_gap_limits_interpolation() {
        let log = parse_gpx(LOG).unwrap();
        // the points are 60 s apart: with a 20 s limit, only the nearest point within 20 s counts
        assert!(log.locate(t("2026-05-01T10:00:30Z"), 20.0).is_none());
        let (g, m) = log.locate(t("2026-05-01T10:00:50Z"), 20.0).unwrap();
        assert_eq!((m, g.latitude), (Match::Nearest, 46.0010));
    }

    #[test]
    fn gpx_1_0_prefixes_and_zones() {
        let s = r#"<gpx:gpx version="1.0" xmlns:gpx="http://www.topografix.com/GPX/1/0"><gpx:trk><gpx:trkseg>
            <gpx:trkpt lat="-33.9" lon="151.2"><gpx:time>2026-01-02T10:00:00+11:00</gpx:time></gpx:trkpt>
            <gpx:trkpt lat="-33.8" lon="151.3"><gpx:time>2026-01-01T23:01:00</gpx:time></gpx:trkpt>
        </gpx:trkseg></gpx:trk></gpx:gpx>"#;
        let log = parse_gpx(s).unwrap();
        assert_eq!(log.points.len(), 2);
        // +11:00 is converted to UTC; no zone = UTC
        assert_eq!(log.points[1].time - log.points[0].time, 60.0);
    }

    #[test]
    fn interpolation_across_the_antimeridian() {
        let a = TrackPoint {
            time: 0.0,
            latitude: 0.0,
            longitude: 179.0,
            elevation: None,
            segment: 0,
        };
        let b = TrackPoint {
            time: 10.0,
            latitude: 0.0,
            longitude: -179.0,
            elevation: None,
            segment: 0,
        };
        let g = lerp(&a, &b, 0.75);
        assert!((g.longitude - -179.5).abs() < 1e-9, "{g:?}");
    }

    #[test]
    fn rejects_non_gpx_and_bad_input() {
        assert_eq!(parse_gpx("<kml><Document/></kml>"), Err(GpxError::NotGpx));
        assert!(parse_gpx("").is_err());
        assert!(
            parse_gpx("<gpx><trk><trkseg><trkpt lat=\"x\" lon=\"1\"><time>2026-01-01T00:00:00Z</time></trkpt></trkseg></trk></gpx>")
                .unwrap()
                .points
                .is_empty()
        );
        let deep = "<gpx>".to_string() + &"<a>".repeat(100);
        assert_eq!(parse_gpx(&deep), Err(GpxError::TooComplex));
    }

    #[test]
    fn camera_clock_policy() {
        // 10:00:30 on a clock at UTC+2 is 08:00:30Z
        let u = camera_time_to_unix("2026:05:01 10:00:30", 120, 0.0).unwrap();
        assert_eq!(u, t("2026-05-01T08:00:30Z"));
        // a camera running 90 s fast
        let u = camera_time_to_unix("2026:05:01 10:00:30", 0, 90.0).unwrap();
        assert_eq!(u, t("2026-05-01T09:59:00Z"));
        // an explicit zone in the text wins over the configured offset
        let u = camera_time_to_unix("2026-05-01T10:00:30+02:00", -300, 0.0).unwrap();
        assert_eq!(u, t("2026-05-01T08:00:30Z"));
        assert!(camera_time_to_unix("garbage", 0, 0.0).is_none());
        assert!(camera_time_to_unix("2026:13:01 10:00:30", 0, 0.0).is_none());
    }

    #[test]
    fn bare_dates_and_bad_times_are_rejected() {
        assert!(parse_time("2026-05-01").is_none());
        assert!(parse_time("2026-05-01T25:00:00Z").is_none());
        assert!(parse_time("2026-05-01T10:00:00+0200").is_none());
    }

    #[test]
    fn geotag_writes_the_located_position_into_xmp() {
        let log = parse_gpx(LOG).unwrap();
        let xmp = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="4"/></rdf:RDF>"#;
        // camera at UTC+2: 12:00:30 local = 10:00:30Z, halfway between the first two points
        let opts = GeotagOptions {
            camera_utc_offset_minutes: 120,
            ..Default::default()
        };
        let (out, how) = geotag(xmp, &log, "2026:05:01 12:00:30", &opts).unwrap();
        assert_eq!(how, Match::Interpolated);
        assert!(out.contains("exif:GPSLatitude=\"46,0.030000N\""), "{out}");
        assert!(out.contains("xmp:Rating=\"4\""), "{out}");
        assert!(matches!(
            geotag(xmp, &log, "2026:05:01 20:00:00", &opts),
            Err(GeotagError::NoPosition)
        ));
        assert!(matches!(
            geotag(xmp, &log, "nope", &opts),
            Err(GeotagError::NoCaptureTime)
        ));
    }
}
