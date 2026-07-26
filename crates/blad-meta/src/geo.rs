//! Nearest-city lookup for GPS coordinates.
//!
//! **Entirely offline, deliberately.** Reverse geocoding is normally an HTTP call, which
//! would mean sending the coordinates of wherever a photo was taken — often someone's
//! home — to a third party, as a side effect of reading a file's metadata. A tool that
//! ships a `--redact` flag cannot also leak the thing it redacts. So the table is
//! embedded and nothing here touches the network.
//!
//! The cost is a 240 KB table and approximate answers, which is why [`Nearest`] always
//! carries the distance: "near Woking, GB (6 km)" and "near Ulaanbaatar, MN (210 km)"
//! are both honest, and only one of them is useful. Callers show the number so the
//! reader can judge, rather than being handed a confident city name that is 200 km off.
//!
//! Data: GeoNames (<https://www.geonames.org>), CC BY 4.0, cities with population
//! ≥ 50,000.

/// `u32` count, then records of `i32` lat×1e5, `i32` lon×1e5, `u32` population,
/// `u8` name length, 2-byte country code, and the UTF-8 name.
static CITIES: &[u8] = include_bytes!("../data/cities.bin");

const RECORD_HEAD: usize = 4 + 4 + 4 + 1 + 2;

/// How much further a notably larger place may be and still win.
///
/// The nearest record to the Eiffel Tower is "Paris 16 Passy", an arrondissement, with
/// "Paris 15 Vaugirard" close behind; the answer a person wants is Paris, whose centroid
/// is 4 km away. Both halves of the rule matter: a bare distance rule returns the
/// subdivision, and a bare population rule would return the nearest metropolis from a
/// genuinely small town.
const NOTABILITY_SLACK_KM: f64 = 8.0;

/// …and it must be this much larger to displace the closest match, so a small town is
/// not relabelled as the nearest big city merely for being small.
const NOTABILITY_RATIO: u32 = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct Nearest {
    pub city: String,
    /// ISO 3166-1 alpha-2.
    pub country: String,
    pub km: f64,
}

impl std::fmt::Display for Nearest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Below ~5 km the city name is the honest answer; beyond that, say how far.
        if self.km < 5.0 {
            write!(f, "{}, {}", self.city, self.country)
        } else {
            write!(
                f,
                "near {}, {} ({:.0} km)",
                self.city, self.country, self.km
            )
        }
    }
}

/// Great-circle distance in kilometres.
fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6371.0088;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().asin()
}

/// The closest known city, if one is within `max_km`.
///
/// Returns `None` rather than a very distant city: mid-ocean and deep-desert
/// coordinates have no meaningful nearest city, and naming one 900 km away would be
/// worse than saying nothing.
pub fn nearest(lat: f64, lon: f64, max_km: f64) -> Option<Nearest> {
    if !lat.is_finite() || !lon.is_finite() || !(-90.0..=90.0).contains(&lat) {
        return None;
    }

    let count = u32::from_le_bytes(CITIES[0..4].try_into().ok()?) as usize;
    // Every candidate within range: (km, population, name_off, name_len, cc_off).
    let mut hits: Vec<(f64, u32, usize, usize, usize)> = Vec::new();
    let mut p = 4usize;
    let deg = max_km / 111.0;

    for _ in 0..count {
        if p + RECORD_HEAD > CITIES.len() {
            break;
        }
        let clat = i32::from_le_bytes(CITIES[p..p + 4].try_into().ok()?) as f64 / 100_000.0;
        let clon = i32::from_le_bytes(CITIES[p + 4..p + 8].try_into().ok()?) as f64 / 100_000.0;
        let pop = u32::from_le_bytes(CITIES[p + 8..p + 12].try_into().ok()?);
        let nlen = CITIES[p + 12] as usize;
        let cc = p + 13;
        let name = cc + 2;

        // Cheap rejection before the trigonometry: one degree of latitude is ~111 km,
        // and longitude can only be shorter. Skips almost the whole table.
        if (clat - lat).abs() <= deg {
            let d = haversine(lat, lon, clat, clon);
            if d <= max_km {
                hits.push((d, pop, name, nlen, cc));
            }
        }
        p = name + nlen;
    }

    // The closest match is the default answer.
    let nearest_hit = *hits
        .iter()
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))?;

    // A substantially larger place, barely further away, is the better answer.
    let notable = hits
        .iter()
        .filter(|h| {
            h.0 <= nearest_hit.0 + NOTABILITY_SLACK_KM
                && h.1 >= nearest_hit.1.saturating_mul(NOTABILITY_RATIO)
        })
        .max_by_key(|h| h.1);

    let (km, _, off, len, cc) = *notable.unwrap_or(&nearest_hit);
    Some(Nearest {
        city: String::from_utf8_lossy(CITIES.get(off..off + len)?).into_owned(),
        country: String::from_utf8_lossy(CITIES.get(cc..cc + 2)?).into_owned(),
        km,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_parses() {
        let count = u32::from_le_bytes(CITIES[0..4].try_into().unwrap());
        assert!(count > 10_000, "only {count} cities");
    }

    #[test]
    fn finds_well_known_places() {
        for (lat, lon, want) in [
            (51.5074, -0.1278, "London"),
            (35.6895, 139.6917, "Tokyo"),
            (-33.8688, 151.2093, "Sydney"),
            (40.7128, -74.0060, "New York"),
        ] {
            let n = nearest(lat, lon, 100.0).unwrap_or_else(|| panic!("nothing near {want}"));
            assert!(
                n.city.contains(want),
                "expected {want}, got {} ({:.0} km)",
                n.city,
                n.km
            );
            assert!(n.km < 30.0, "{} was {:.0} km away", n.city, n.km);
        }
    }

    /// The nearest records to the Eiffel Tower are arrondissements. People say "Paris".
    #[test]
    fn notable_cities_beat_their_own_subdivisions() {
        for (lat, lon, want) in [(48.8584, 2.2945, "Paris"), (35.6586, 139.7454, "Tokyo")] {
            let n = nearest(lat, lon, 150.0).unwrap();
            assert_eq!(n.city, want, "got {} at {:.1} km", n.city, n.km);
        }
    }

    /// …but a small town must not be relabelled as the nearest metropolis.
    #[test]
    fn small_towns_keep_their_own_name() {
        // Cambridge, UK: ~80 km from London, must not become "London".
        let n = nearest(52.2053, 0.1218, 150.0).unwrap();
        assert_eq!(n.city, "Cambridge", "got {}", n.city);
    }

    /// Naming a city 900 km away would be worse than admitting we do not know.
    #[test]
    fn empty_ocean_has_no_nearest_city() {
        assert!(nearest(-40.0, -140.0, 200.0).is_none());
    }

    #[test]
    fn rejects_impossible_coordinates() {
        assert!(nearest(f64::NAN, 0.0, 100.0).is_none());
        assert!(nearest(120.0, 0.0, 100.0).is_none());
    }

    /// The distance is the honesty mechanism, so it must be in the text once it matters.
    #[test]
    fn distance_is_shown_when_it_is_not_a_direct_hit() {
        let close = Nearest {
            city: "London".into(),
            country: "GB".into(),
            km: 1.2,
        };
        assert_eq!(close.to_string(), "London, GB");
        let far = Nearest {
            city: "London".into(),
            country: "GB".into(),
            km: 42.0,
        };
        assert_eq!(far.to_string(), "near London, GB (42 km)");
    }
}
