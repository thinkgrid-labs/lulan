//! GTFS importer (Phase 7, §15 migration tooling): `lulan-api import-gtfs
//! <dir>` ingests the schedule feed most operators already have.
//!
//! Reads: agency.txt → operators · stops.txt → locations · routes.txt +
//! trips.txt + stop_times.txt → routes with per-stop schedule offsets ·
//! calendar.txt → concrete dated trips.
//!
//! Honest v1 scope (documented, not hidden):
//! - Each distinct stop pattern of a GTFS route becomes its own Lulan
//!   route (`<short_name>-<n>`) — that's how direction 0/1 and branch
//!   variants map onto Lulan's linear segment model.
//! - Times are taken as-is (no timezone math): `departs_at` = service
//!   date + first-stop departure, treated as UTC. `HH:MM:SS` ≥ 24:00
//!   wraps to the next day, per the GTFS spec.
//! - `calendar_dates.txt` exceptions and frequencies.txt are not yet
//!   applied (skipped with a warning if present).
//! - GTFS carries no seat maps: trips attach to a vehicle you describe
//!   (`--seats N`, rows of 4, economy) or an existing resource
//!   (`--vessel CODE`).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, bail};
use chrono::{Datelike, Duration, NaiveDate, NaiveTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

pub struct GtfsOptions {
    /// Expand dated trips for this many days starting today.
    pub days: i64,
    /// Seats on the auto-created vehicle (rows of 4, economy).
    pub seats: u32,
    /// Reuse an existing resource by code instead of creating one.
    pub vessel: Option<String>,
}

impl Default for GtfsOptions {
    fn default() -> Self {
        Self {
            days: 30,
            seats: 40,
            vessel: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Agency {
    #[serde(default)]
    agency_id: String,
    agency_name: String,
}

#[derive(Debug, Deserialize)]
struct Stop {
    stop_id: String,
    stop_name: String,
    #[serde(default)]
    stop_timezone: String,
}

#[derive(Debug, Deserialize)]
struct RouteRow {
    route_id: String,
    #[serde(default)]
    route_short_name: String,
    #[serde(default)]
    route_long_name: String,
}

#[derive(Debug, Deserialize)]
struct TripRow {
    route_id: String,
    service_id: String,
    trip_id: String,
}

#[derive(Debug, Deserialize)]
struct StopTime {
    trip_id: String,
    arrival_time: String,
    departure_time: String,
    stop_id: String,
    stop_sequence: u32,
}

#[derive(Debug, Deserialize)]
struct Calendar {
    service_id: String,
    monday: u8,
    tuesday: u8,
    wednesday: u8,
    thursday: u8,
    friday: u8,
    saturday: u8,
    sunday: u8,
    start_date: String,
    end_date: String,
}

fn read<T: for<'de> Deserialize<'de>>(dir: &Path, file: &str) -> anyhow::Result<Vec<T>> {
    let path = dir.join(file);
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .flexible(true)
        .from_path(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    reader
        .deserialize()
        .collect::<Result<Vec<T>, _>>()
        .with_context(|| format!("parsing {file}"))
}

/// GTFS times exceed 23:59:59 for after-midnight service; returns
/// (minutes past service-day midnight).
fn gtfs_minutes(time: &str) -> anyhow::Result<i32> {
    let mut parts = time.split(':');
    let (Some(h), Some(m)) = (parts.next(), parts.next()) else {
        bail!("bad GTFS time {time:?}");
    };
    Ok(h.parse::<i32>()? * 60 + m.parse::<i32>()?)
}

fn gtfs_date(date: &str) -> anyhow::Result<NaiveDate> {
    NaiveDate::parse_from_str(date, "%Y%m%d").with_context(|| format!("bad GTFS date {date:?}"))
}

pub async fn import(pool: &PgPool, dir: &Path, options: GtfsOptions) -> anyhow::Result<()> {
    let agencies: Vec<Agency> = read(dir, "agency.txt")?;
    let stops: Vec<Stop> = read(dir, "stops.txt")?;
    let routes: Vec<RouteRow> = read(dir, "routes.txt")?;
    let trips: Vec<TripRow> = read(dir, "trips.txt")?;
    let mut stop_times: Vec<StopTime> = read(dir, "stop_times.txt")?;
    let calendars: Vec<Calendar> = read(dir, "calendar.txt")?;
    for optional in ["calendar_dates.txt", "frequencies.txt"] {
        if dir.join(optional).exists() {
            eprintln!("warning: {optional} present but not applied (v1 importer scope)");
        }
    }

    let mut tx = pool.begin().await?;

    // ---- Operator (first agency) --------------------------------------
    let agency = agencies.first().context("agency.txt has no rows")?;
    let operator_code = if agency.agency_id.is_empty() {
        agency
            .agency_name
            .chars()
            .filter(|c| c.is_alphanumeric())
            .take(8)
            .collect::<String>()
            .to_uppercase()
    } else {
        agency.agency_id.clone()
    };
    let operator_id: Uuid = sqlx::query_scalar(
        "INSERT INTO operators (id, code, name) VALUES ($1, $2, $3)
         ON CONFLICT (code) DO UPDATE SET name = excluded.name
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(&operator_code)
    .bind(&agency.agency_name)
    .fetch_one(&mut *tx)
    .await?;

    // ---- Locations -----------------------------------------------------
    let mut location_ids: HashMap<String, Uuid> = HashMap::new();
    for stop in &stops {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO locations (id, code, name, timezone) VALUES ($1, $2, $3, $4)
             ON CONFLICT (code) DO UPDATE SET name = excluded.name
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(&stop.stop_id)
        .bind(&stop.stop_name)
        .bind(if stop.stop_timezone.is_empty() {
            "UTC"
        } else {
            &stop.stop_timezone
        })
        .fetch_one(&mut *tx)
        .await?;
        location_ids.insert(stop.stop_id.clone(), id);
    }

    // ---- Vehicle (capacity template) ------------------------------------
    let resource_id: Uuid = match &options.vessel {
        Some(code) => sqlx::query_scalar("SELECT id FROM resources WHERE code = $1")
            .bind(code)
            .fetch_optional(&mut *tx)
            .await?
            .with_context(|| format!("--vessel {code}: no such resource"))?,
        None => {
            let code = format!("GTFS-{operator_code}");
            let existing: Option<Uuid> =
                sqlx::query_scalar("SELECT id FROM resources WHERE code = $1")
                    .bind(&code)
                    .fetch_optional(&mut *tx)
                    .await?;
            match existing {
                Some(id) => id,
                None => {
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO resources (id, code, name, kind) VALUES ($1, $2, $3, 'bus')",
                    )
                    .bind(id)
                    .bind(&code)
                    .bind(format!("{} vehicle", agency.agency_name))
                    .execute(&mut *tx)
                    .await?;
                    let rows = options.seats.div_ceil(4).max(1);
                    let mut created = 0u32;
                    for row in 1..=rows {
                        for letter in ['A', 'B', 'C', 'D'] {
                            if created == options.seats {
                                break;
                            }
                            sqlx::query(
                                "INSERT INTO capacity_units (id, resource_id, kind, code, fare_class)
                                 VALUES ($1, $2, 'seat', $3, 'economy')",
                            )
                            .bind(Uuid::new_v4())
                            .bind(id)
                            .bind(format!("{row}{letter}"))
                            .execute(&mut *tx)
                            .await?;
                            created += 1;
                        }
                    }
                    id
                }
            }
        }
    };
    let seat_units: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM capacity_units WHERE resource_id = $1 AND kind = 'seat'",
    )
    .bind(resource_id)
    .fetch_all(&mut *tx)
    .await?;

    // ---- Stop patterns → Lulan routes ------------------------------------
    stop_times.sort_by(|a, b| {
        a.trip_id
            .cmp(&b.trip_id)
            .then(a.stop_sequence.cmp(&b.stop_sequence))
    });
    let mut trip_stops: HashMap<&str, Vec<&StopTime>> = HashMap::new();
    for st in &stop_times {
        trip_stops.entry(&st.trip_id).or_default().push(st);
    }

    let route_names: HashMap<&str, String> = routes
        .iter()
        .map(|r| {
            let name = if r.route_short_name.is_empty() {
                r.route_long_name.clone()
            } else {
                r.route_short_name.clone()
            };
            (r.route_id.as_str(), name)
        })
        .collect();

    // pattern key = (gtfs route, ordered stop ids) → lulan route id + the
    // offsets derived from the first trip seen with that pattern.
    struct Pattern {
        route_id: Uuid,
        first_departure_min: i32,
    }
    let mut patterns: HashMap<(String, Vec<String>), Pattern> = HashMap::new();
    let mut pattern_count_per_route: HashMap<String, u32> = HashMap::new();
    let mut skipped_trips = 0usize;
    let mut dated_trips = 0usize;

    let services: HashMap<&str, &Calendar> = calendars
        .iter()
        .map(|c| (c.service_id.as_str(), c))
        .collect();
    let today = Utc::now().date_naive();
    let horizon = today + Duration::days(options.days);

    for trip in &trips {
        let Some(stops_of_trip) = trip_stops.get(trip.trip_id.as_str()) else {
            skipped_trips += 1;
            continue;
        };
        if stops_of_trip.len() < 2 || stops_of_trip.len() > 64 {
            skipped_trips += 1;
            continue;
        }
        let stop_ids: Vec<String> = stops_of_trip.iter().map(|s| s.stop_id.clone()).collect();
        let key = (trip.route_id.clone(), stop_ids.clone());

        // Create the Lulan route for a new pattern.
        if !patterns.contains_key(&key) {
            let n = pattern_count_per_route
                .entry(trip.route_id.clone())
                .or_insert(0);
            let base_name = route_names
                .get(trip.route_id.as_str())
                .cloned()
                .unwrap_or_else(|| trip.route_id.clone());
            let code = format!("{base_name}-{n}");
            *n += 1;

            let first_departure = gtfs_minutes(&stops_of_trip[0].departure_time)?;
            let route_id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO routes (id, code, name) VALUES ($1, $2, $3)
                 ON CONFLICT (code) DO NOTHING",
            )
            .bind(route_id)
            .bind(&code)
            .bind(&base_name)
            .execute(&mut *tx)
            .await?;
            // If the code already existed (re-import), reuse it.
            let route_id: Uuid = sqlx::query_scalar("SELECT id FROM routes WHERE code = $1")
                .bind(&code)
                .fetch_one(&mut *tx)
                .await?;

            for (index, st) in stops_of_trip.iter().enumerate() {
                let location = location_ids.get(&st.stop_id).with_context(|| {
                    format!("stop_times references unknown stop {}", st.stop_id)
                })?;
                sqlx::query(
                    "INSERT INTO route_stops (route_id, stop_index, location_id, arrive_offset_min, depart_offset_min)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (route_id, stop_index) DO NOTHING",
                )
                .bind(route_id)
                .bind(index as i16)
                .bind(location)
                .bind(gtfs_minutes(&st.arrival_time)? - first_departure)
                .bind(gtfs_minutes(&st.departure_time)? - first_departure)
                .execute(&mut *tx)
                .await?;
            }
            patterns.insert(
                key.clone(),
                Pattern {
                    route_id,
                    first_departure_min: first_departure,
                },
            );
        }
        let pattern = &patterns[&key];

        // Expand this trip's service into dated departures in the window.
        let Some(service) = services.get(trip.service_id.as_str()) else {
            skipped_trips += 1;
            continue;
        };
        let start = gtfs_date(&service.start_date)?.max(today);
        let end = gtfs_date(&service.end_date)?.min(horizon);
        let weekdays = [
            service.monday,
            service.tuesday,
            service.wednesday,
            service.thursday,
            service.friday,
            service.saturday,
            service.sunday,
        ];
        let segment_count = (stop_ids.len() - 1) as i16;
        let mut date = start;
        while date <= end {
            if weekdays[date.weekday().num_days_from_monday() as usize] == 1 {
                let departs_at = date.and_time(NaiveTime::MIN).and_utc()
                    + Duration::minutes(pattern.first_departure_min as i64);
                let trip_uuid = Uuid::new_v4();
                let inserted = sqlx::query(
                    "INSERT INTO trips (id, route_id, resource_id, operator_id, service_number, service_date, departs_at, segment_count)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                     ON CONFLICT (route_id, resource_id, departs_at) DO NOTHING",
                )
                .bind(trip_uuid)
                .bind(pattern.route_id)
                .bind(resource_id)
                .bind(operator_id)
                .bind(&trip.trip_id)
                .bind(date)
                .bind(departs_at)
                .bind(segment_count)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if inserted == 1 {
                    dated_trips += 1;
                    for unit in &seat_units {
                        sqlx::query(
                            "INSERT INTO seat_occupancy (trip_id, unit_id, occupied_mask) VALUES ($1, $2, 0)",
                        )
                        .bind(trip_uuid)
                        .bind(unit)
                        .execute(&mut *tx)
                        .await?;
                    }
                }
            }
            date += Duration::days(1);
        }
    }

    tx.commit().await?;
    println!(
        "GTFS import: {} operator, {} stops, {} route patterns, {dated_trips} dated trips ({} source trips skipped)",
        agency.agency_name,
        stops.len(),
        patterns.len(),
        skipped_trips
    );
    Ok(())
}
