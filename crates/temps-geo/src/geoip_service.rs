use maxminddb::geoip2;
use memmap2::Mmap;
use rand::prelude::IndexedRandom;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Debug, Serialize, Clone)]
pub struct GeoLocation {
    pub country: Option<String>,
    pub country_code: Option<String>,
    pub city: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub region: Option<String>,
    pub timezone: Option<String>,
    pub is_eu: bool,
    /// The organization that owns the ASN this IP belongs to (e.g. "Hetzner Online GmbH").
    /// `None` when the ASN database isn't available or the ASN has no organization on file.
    pub asn_org: Option<String>,
    /// Best-effort heuristic: does `asn_org` match a known hosting/cloud/VPS provider
    /// pattern? Datacenter IPs are never real end-user visitors, so this flags traffic
    /// (often scrapers spoofing a real browser user-agent) that the user-agent-based
    /// bot detector in temps-analytics-events can't catch. `None` when the ASN database
    /// isn't available (can't tell either way).
    pub is_hosting_provider: Option<bool>,
}

/// Substring patterns (lowercased) matched against an ASN organization name to
/// flag traffic originating from hosting/cloud/VPS providers rather than
/// residential or mobile ISPs. Best-effort: a legitimate visitor tunneling
/// through a corporate VPN hosted on one of these providers would also match,
/// so this is a signal for the live-visitors view, not a hard security boundary.
const HOSTING_ORG_PATTERNS: &[&str] = &[
    "hosting",
    "vps",
    "datacenter",
    "data center",
    "colocation",
    "cloud",
    "server",
    "amazon",
    "aws",
    "google cloud",
    "microsoft azure",
    "digitalocean",
    "linode",
    "vultr",
    "ovh",
    "hetzner",
    "leaseweb",
    "scaleway",
    "contabo",
    "packet",
    "equinix",
    "choopa",
    "he.net",
    "hurricane electric",
    "subnet digital",
    "americancloud",
];

/// Detect whether an ASN organization name belongs to a known hosting/cloud/VPS
/// provider. Returns `false` for an empty name rather than matching everything.
fn is_hosting_org(org: &str) -> bool {
    let trimmed = org.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_lowercase();
    HOSTING_ORG_PATTERNS.iter().any(|pat| lower.contains(pat))
}

#[derive(Error, Debug)]
pub enum GeoIpError {
    #[error("Failed to open MaxMind database: {0}")]
    DatabaseError(#[from] maxminddb::MaxMindDbError),
    #[error("IP address not found in database")]
    NotFound(String),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("MaxMind database at '{path}' is unusable: {reason}")]
    DatabaseUnusable { path: String, reason: String },
    #[error("Other error: {0}")]
    Other(String),
}

/// Sample cities for mock geolocation data
#[derive(Clone)]
struct MockCity {
    city: &'static str,
    region: &'static str,
    country: &'static str,
    country_code: &'static str,
    latitude: f64,
    longitude: f64,
    timezone: &'static str,
    is_eu: bool,
}

const MOCK_CITIES: &[MockCity] = &[
    MockCity {
        city: "New York",
        region: "New York",
        country: "United States",
        country_code: "US",
        latitude: 40.7128,
        longitude: -74.0060,
        timezone: "America/New_York",
        is_eu: false,
    },
    MockCity {
        city: "London",
        region: "England",
        country: "United Kingdom",
        country_code: "GB",
        latitude: 51.5074,
        longitude: -0.1278,
        timezone: "Europe/London",
        is_eu: false,
    },
    MockCity {
        city: "Paris",
        region: "Île-de-France",
        country: "France",
        country_code: "FR",
        latitude: 48.8566,
        longitude: 2.3522,
        timezone: "Europe/Paris",
        is_eu: true,
    },
    MockCity {
        city: "Tokyo",
        region: "Tokyo",
        country: "Japan",
        country_code: "JP",
        latitude: 35.6762,
        longitude: 139.6503,
        timezone: "Asia/Tokyo",
        is_eu: false,
    },
    MockCity {
        city: "Sydney",
        region: "New South Wales",
        country: "Australia",
        country_code: "AU",
        latitude: -33.8688,
        longitude: 151.2093,
        timezone: "Australia/Sydney",
        is_eu: false,
    },
    MockCity {
        city: "Berlin",
        region: "Berlin",
        country: "Germany",
        country_code: "DE",
        latitude: 52.5200,
        longitude: 13.4050,
        timezone: "Europe/Berlin",
        is_eu: true,
    },
    MockCity {
        city: "Toronto",
        region: "Ontario",
        country: "Canada",
        country_code: "CA",
        latitude: 43.6532,
        longitude: -79.3832,
        timezone: "America/Toronto",
        is_eu: false,
    },
    MockCity {
        city: "Singapore",
        region: "Singapore",
        country: "Singapore",
        country_code: "SG",
        latitude: 1.3521,
        longitude: 103.8198,
        timezone: "Asia/Singapore",
        is_eu: false,
    },
];

pub enum GeoIpService {
    MaxMind(Arc<MaxMindGeoIpService>),
    Mock(MockGeoIpService),
}

/// Readers already loaded in this process, keyed by the resolved City database
/// path and held weakly so the memo never outlives its last consumer.
///
/// `temps serve` builds two independent plugin registries in one process -- one
/// for the proxy (`temps_proxy::server::setup_proxy_server`) and one for the
/// console API (`start_console_api`) -- and each registers its own `GeoPlugin`.
/// Without this memo each registration calls `Reader::open_readfile`, which
/// reads the whole database into a fresh `Vec<u8>`, so a single-node instance
/// holds two full copies of GeoLite2-City.mmdb on the heap.
static LOADED_DATABASES: LazyLock<Mutex<HashMap<PathBuf, Weak<MaxMindGeoIpService>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Returns the cached value for `key`, or inserts what `load` produces.
///
/// The lock is deliberately held across `load`: the proxy and console register
/// their geo plugins concurrently, so releasing it first would let both observe
/// a miss and each read the database anyway -- the exact duplication this memo
/// exists to prevent. `load` is startup-only file I/O, never on a request path.
/// A poisoned lock only means some earlier caller panicked while loading; the
/// map itself is still a consistent cache, so recover it rather than propagate.
fn get_or_load<T, F>(
    cache: &Mutex<HashMap<PathBuf, Weak<T>>>,
    key: PathBuf,
    load: F,
) -> Result<Arc<T>, GeoIpError>
where
    F: FnOnce() -> Result<T, GeoIpError>,
{
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = cache.get(&key).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let loaded = Arc::new(load()?);
    cache.insert(key, Arc::downgrade(&loaded));
    Ok(loaded)
}

/// Matches the startup downloader's own floor: below this a file is truncated,
/// not a database.
const MIN_PLAUSIBLE_MMDB_BYTES: u64 = 1_000_000;

/// Backing bytes for a `maxminddb::Reader`. `Mapped` is clean, evictable page
/// cache; `Heap` is the anonymous fallback, identical to `Reader::open_readfile`.
enum DatabaseSource {
    Mapped(Mmap),
    Heap(Vec<u8>),
}

impl AsRef<[u8]> for DatabaseSource {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Mapped(mmap) => mmap.as_ref(),
            Self::Heap(buf) => buf.as_ref(),
        }
    }
}

impl DatabaseSource {
    fn kind(&self) -> &'static str {
        match self {
            Self::Mapped(_) => "mmap",
            Self::Heap(_) => "heap",
        }
    }
}

/// Opens `path` and checks what `Mmap::map` requires but does not verify.
///
/// Metadata is read through the returned descriptor, so the checks apply to the
/// inode that gets mapped and cannot be invalidated by a concurrent rename.
fn open_verified_database(path: &Path) -> Result<(File, u64), GeoIpError> {
    let file = File::open(path).map_err(|e| GeoIpError::DatabaseUnusable {
        path: path.display().to_string(),
        reason: format!("could not be opened: {e}"),
    })?;

    let metadata = file.metadata().map_err(|e| GeoIpError::DatabaseUnusable {
        path: path.display().to_string(),
        reason: format!("could not be stat'ed through its own descriptor: {e}"),
    })?;

    // Devices, FIFOs and procfs entries have no fixed length, so a mapping's
    // bounds would not describe the data.
    if !metadata.is_file() {
        return Err(GeoIpError::DatabaseUnusable {
            path: path.display().to_string(),
            reason: format!(
                "is not a regular file (found {:?}); refusing to map it",
                metadata.file_type()
            ),
        });
    }

    let len = metadata.len();
    if len < MIN_PLAUSIBLE_MMDB_BYTES {
        return Err(GeoIpError::DatabaseUnusable {
            path: path.display().to_string(),
            reason: format!(
                "is {len} bytes, below the {MIN_PLAUSIBLE_MMDB_BYTES}-byte minimum; \
                 treating it as truncated"
            ),
        });
    }

    Ok((file, len))
}

/// Maps the database, falling back to a heap read if mapping is unavailable.
///
/// Read onto the heap, GeoLite2-City.mmdb is ~58 MiB of anonymous memory:
/// always resident, reclaimable only via swap. Mapped, only the pages a lookup
/// walks become resident and the kernel can drop them for free.
fn load_database_source(path: &Path) -> Result<DatabaseSource, GeoIpError> {
    let (file, len) = open_verified_database(path)?;

    // SAFETY: mapping aliases bytes another process could change; shrinking the
    // file would make access raise SIGBUS, rewriting it would break the
    // immutability `&[u8]` promises. Discharged by:
    // 1. `open_verified_database` confirmed a regular file of known length
    //    through this very descriptor, so no check/map TOCTOU.
    // 2. `Mmap` keeps its own descriptor, so the inode stays alive while mapped
    //    even if the directory entry is replaced or removed.
    // 3. The only writer, `download_geolite2_database_on_startup` in temps-cli,
    //    streams to `.mmdb.tmp` and `rename`s over the path; by (2) this mapping
    //    stays bound to the old inode, whose bytes never change.
    // 4. Nothing reopens the database at runtime -- `GeoIpService::new` is
    //    reached only from plugin registration, and there is no refresh job.
    //
    // Residual: an operator overwriting in place (`cp` truncates, `mv` does not)
    // on a running server. Outside this process's control; see docs.
    let mapped = unsafe { Mmap::map(&file) };

    match mapped {
        Ok(mmap) => {
            // Deliberately no `advise(Random)`: measured cold, it trades 2.7 MB
            // of resident pages for 2x lookup latency by suppressing readahead.
            debug!("Mapped MaxMind database '{}' ({len} bytes)", path.display());
            Ok(DatabaseSource::Mapped(mmap))
        }
        Err(e) => {
            warn!(
                "Could not memory-map '{}' ({e}); reading it instead, costing ~{} MiB anonymous",
                path.display(),
                len / (1024 * 1024)
            );
            let buf = std::fs::read(path).map_err(|e| GeoIpError::DatabaseUnusable {
                path: path.display().to_string(),
                reason: format!("could not be read after mapping failed: {e}"),
            })?;
            Ok(DatabaseSource::Heap(buf))
        }
    }
}

/// Resolves an mmdb filename the same way `validate_geolite2_database`
/// (temps-cli's serve startup check) does: current working directory first,
/// falling back to `TEMPS_DATA_DIR` if the CWD candidate doesn't exist. The
/// two resolution orders must stay in sync -- when they diverged (this
/// function previously only checked CWD), `temps serve` could pass its own
/// startup validation (which checks + downloads to `TEMPS_DATA_DIR`) and
/// then fail to actually open the database here, because a process whose
/// working directory isn't its data directory (e.g. any Docker/systemd
/// deployment that doesn't `cd` into `TEMPS_DATA_DIR` before running) would
/// have downloaded the file to `TEMPS_DATA_DIR` but only ever looked in CWD.
pub(crate) fn resolve_mmdb_path(filename: &str) -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    let data_dir = std::env::var_os("TEMPS_DATA_DIR").map(std::path::PathBuf::from);
    resolve_mmdb_path_from(filename, &cwd, data_dir.as_deref())
}

fn resolve_mmdb_path_from(
    filename: &str,
    cwd: &std::path::Path,
    data_dir: Option<&std::path::Path>,
) -> std::path::PathBuf {
    let cwd_path = cwd.join(filename);
    if cwd_path.exists() {
        return cwd_path;
    }

    // When neither file exists yet, prefer the configured data directory:
    // startup downloads there. Returning the absent CWD candidate would make
    // the plugin wait loop watch a path that can never receive the download.
    data_dir.map(|path| path.join(filename)).unwrap_or(cwd_path)
}

impl GeoIpService {
    pub fn new() -> Result<Self, GeoIpError> {
        // Check if we should use mock service for local development
        let use_mock = std::env::var("TEMPS_GEO_MOCK")
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);

        if use_mock {
            info!("Using mock GeoIP service for local development");
            return Ok(Self::Mock(MockGeoIpService::new()));
        }

        let db_path = resolve_mmdb_path("GeoLite2-City.mmdb");
        let service = get_or_load(&LOADED_DATABASES, db_path.clone(), || {
            debug!("Loading MaxMind database from: {:?}", db_path);
            let source = load_database_source(&db_path)?;
            let source_kind = source.kind();
            let reader = maxminddb::Reader::from_source(source)?;
            info!(
                "Loaded MaxMind City database from {:?} ({source_kind})",
                db_path
            );

            // ASN database is optional: hosting-provider detection degrades gracefully
            // (asn_org/is_hosting_provider stay None) rather than failing startup when
            // the operator hasn't provisioned it, same as the City database's own
            // optional-file convention in Dockerfile/docker-compose.
            let asn_db_path = resolve_mmdb_path("GeoLite2-ASN.mmdb");
            let asn_reader = match load_database_source(&asn_db_path)
                .and_then(|source| Ok(maxminddb::Reader::from_source(source)?))
            {
                Ok(reader) => {
                    info!("Loaded MaxMind ASN database from: {:?}", asn_db_path);
                    Some(reader)
                }
                Err(e) => {
                    warn!(
                        "MaxMind ASN database not found at '{}' ({}); hosting-provider detection disabled",
                        asn_db_path.display(),
                        e
                    );
                    None
                }
            };

            Ok(MaxMindGeoIpService { reader, asn_reader })
        })?;

        Ok(Self::MaxMind(service))
    }

    pub async fn geolocate(&self, ip: IpAddr) -> Result<GeoLocation, GeoIpError> {
        match self {
            Self::MaxMind(service) => service.geolocate(ip).await,
            Self::Mock(service) => service.geolocate(ip).await,
        }
    }
}

pub struct MaxMindGeoIpService {
    reader: maxminddb::Reader<DatabaseSource>,
    asn_reader: Option<maxminddb::Reader<DatabaseSource>>,
}

impl MaxMindGeoIpService {
    pub async fn geolocate(&self, ip: IpAddr) -> Result<GeoLocation, GeoIpError> {
        info!("Geolocating IP: {}", ip);

        let lookup_result = self.reader.lookup(ip)?;

        let city_data = lookup_result
            .decode::<geoip2::City>()
            .map_err(|e| GeoIpError::NotFound(format!("Failed to decode city data: {}", e)))?
            .ok_or_else(|| GeoIpError::NotFound(format!("No data found for IP: {}", ip)))?;

        let mut geo_location = Self::extract_geo_location(&city_data);
        let (asn_org, is_hosting_provider) = self.lookup_asn(ip);
        geo_location.asn_org = asn_org;
        geo_location.is_hosting_provider = is_hosting_provider;

        Ok(geo_location)
    }

    /// Look up the ASN organization for `ip` and classify it as a hosting
    /// provider. Any lookup/decode failure degrades to `(None, None)` rather
    /// than failing the whole geolocation — the City lookup already succeeded
    /// and callers shouldn't lose city/country data over an optional signal.
    fn lookup_asn(&self, ip: IpAddr) -> (Option<String>, Option<bool>) {
        let Some(asn_reader) = &self.asn_reader else {
            return (None, None);
        };

        let asn_data = match asn_reader
            .lookup(ip)
            .and_then(|result| result.decode::<geoip2::Asn>())
        {
            Ok(Some(data)) => data,
            Ok(None) => return (None, Some(false)),
            Err(e) => {
                debug!("ASN lookup failed for IP {}: {}", ip, e);
                return (None, None);
            }
        };

        let org = asn_data.autonomous_system_organization.map(str::to_string);
        let is_hosting = org.as_deref().map(is_hosting_org);

        (org, is_hosting)
    }

    fn extract_geo_location(city_data: &geoip2::City<'_>) -> GeoLocation {
        // Extract country information
        let country = city_data.country.names.english.map(String::from);
        let country_code = city_data.country.iso_code.map(String::from);
        let is_eu = city_data.country.is_in_european_union.unwrap_or(false);

        // Extract city name
        let city_name = city_data.city.names.english.map(String::from);

        // Extract region from first subdivision
        let region = city_data
            .subdivisions
            .first()
            .and_then(|sub| sub.names.english)
            .map(String::from);

        // Extract location data
        let latitude = city_data.location.latitude;
        let longitude = city_data.location.longitude;
        let timezone = city_data.location.time_zone.map(String::from);

        GeoLocation {
            country,
            country_code,
            city: city_name,
            latitude,
            longitude,
            region,
            timezone,
            is_eu,
            asn_org: None,
            is_hosting_provider: None,
        }
    }
}

/// Mock GeoIP service for local development
pub struct MockGeoIpService;

impl Default for MockGeoIpService {
    fn default() -> Self {
        Self::new()
    }
}

impl MockGeoIpService {
    pub fn new() -> Self {
        Self
    }

    pub async fn geolocate(&self, ip: IpAddr) -> Result<GeoLocation, GeoIpError> {
        // For localhost/private IPs, return random city data
        if ip.is_loopback() || Self::is_private_ip(&ip) {
            info!("Mock geolocating IP: {} (localhost/private)", ip);
            return Self::random_mock_location();
        }

        // For other IPs, return a generic response
        info!("Mock geolocating IP: {} (external)", ip);
        Ok(GeoLocation {
            country: Some("Unknown".to_string()),
            country_code: Some("XX".to_string()),
            city: Some("Unknown".to_string()),
            latitude: Some(0.0),
            longitude: Some(0.0),
            region: Some("Unknown".to_string()),
            timezone: Some("UTC".to_string()),
            is_eu: false,
            asn_org: None,
            is_hosting_provider: None,
        })
    }

    fn random_mock_location() -> Result<GeoLocation, GeoIpError> {
        let mut rng = rand::rng();
        let mock_city = MOCK_CITIES
            .choose(&mut rng)
            .ok_or_else(|| GeoIpError::Other("Failed to select mock city".to_string()))?;

        Ok(GeoLocation {
            country: Some(mock_city.country.to_string()),
            country_code: Some(mock_city.country_code.to_string()),
            city: Some(mock_city.city.to_string()),
            latitude: Some(mock_city.latitude),
            longitude: Some(mock_city.longitude),
            region: Some(mock_city.region.to_string()),
            timezone: Some(mock_city.timezone.to_string()),
            is_eu: mock_city.is_eu,
            asn_org: None,
            is_hosting_provider: None,
        })
    }

    fn is_private_ip(ip: &IpAddr) -> bool {
        match ip {
            IpAddr::V4(ipv4) => ipv4.is_private() || ipv4.is_link_local(),
            IpAddr::V6(ipv6) => ipv6.is_loopback() || ipv6.is_unique_local(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_verified_database_rejects_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = open_verified_database(dir.path()).expect_err("a directory is not mappable");
        let msg = err.to_string();
        assert!(msg.contains("not a regular file"), "unexpected: {msg}");
    }

    #[test]
    fn test_open_verified_database_rejects_truncated_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("GeoLite2-City.mmdb");
        std::fs::write(&path, b"not a real database").expect("write");
        let err = open_verified_database(&path).expect_err("a stub file is not a database");
        let msg = err.to_string();
        assert!(
            msg.contains("treating it as truncated"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn test_open_verified_database_reports_missing_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("absent.mmdb");
        let err = open_verified_database(&path).expect_err("missing file");
        let msg = err.to_string();
        assert!(msg.contains("could not be opened"), "unexpected: {msg}");
        assert!(
            msg.contains("absent.mmdb"),
            "error must name the path: {msg}"
        );
    }

    /// Both variants must hand the reader the identical bytes, so the mmap and
    /// fallback paths cannot diverge in what they parse.
    #[test]
    fn test_database_source_exposes_same_bytes_for_both_variants() {
        let bytes = vec![1u8, 2, 3, 4, 5];
        let heap = DatabaseSource::Heap(bytes.clone());
        assert_eq!(heap.as_ref(), &bytes[..]);
        assert_eq!(heap.kind(), "heap");

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bytes.bin");
        std::fs::write(&path, &bytes).expect("write");
        let file = File::open(&path).expect("open");
        // SAFETY: the file is created by this test, lives in a private temp dir
        // that nothing else can reach, and is not written again while mapped.
        let mapped = DatabaseSource::Mapped(unsafe { Mmap::map(&file) }.expect("map"));
        assert_eq!(mapped.as_ref(), heap.as_ref());
        assert_eq!(mapped.kind(), "mmap");
    }

    /// A second load of the same database path must reuse the first reader
    /// rather than allocate another copy. This is the whole point of the memo:
    /// `temps serve` registers `GeoPlugin` once for the proxy and once for the
    /// console API, and GeoLite2-City.mmdb is ~58 MiB per copy.
    #[test]
    fn test_get_or_load_reuses_live_entry_for_same_path() {
        let cache: Mutex<HashMap<PathBuf, Weak<String>>> = Mutex::new(HashMap::new());
        let path = PathBuf::from("/data/GeoLite2-City.mmdb");

        let first = get_or_load(&cache, path.clone(), || Ok("loaded once".to_string()))
            .expect("first load should succeed");
        let second = get_or_load(&cache, path.clone(), || {
            panic!("second load must come from the memo, not re-read the database")
        })
        .expect("second load should hit the memo");

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(Arc::strong_count(&first), 2);
    }

    #[test]
    fn test_get_or_load_keys_on_path() {
        let cache: Mutex<HashMap<PathBuf, Weak<String>>> = Mutex::new(HashMap::new());

        let a = get_or_load(
            &cache,
            PathBuf::from("/a/City.mmdb"),
            || Ok("a".to_string()),
        )
        .expect("load a");
        let b = get_or_load(
            &cache,
            PathBuf::from("/b/City.mmdb"),
            || Ok("b".to_string()),
        )
        .expect("load b");

        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(*a, "a");
        assert_eq!(*b, "b");
    }

    /// The memo holds `Weak`, so once every consumer drops its handle the
    /// database is freed and a later caller reloads it instead of resurrecting
    /// a dangling entry.
    #[test]
    fn test_get_or_load_reloads_after_all_handles_dropped() {
        let cache: Mutex<HashMap<PathBuf, Weak<String>>> = Mutex::new(HashMap::new());
        let path = PathBuf::from("/data/GeoLite2-City.mmdb");

        let first = get_or_load(&cache, path.clone(), || Ok("first".to_string()))
            .expect("first load should succeed");
        drop(first);

        let reloaded = get_or_load(&cache, path.clone(), || Ok("second".to_string()))
            .expect("reload after drop should succeed");
        assert_eq!(*reloaded, "second");
    }

    /// A failed load must not poison the memo: the next caller retries.
    #[test]
    fn test_get_or_load_does_not_cache_failures() {
        let cache: Mutex<HashMap<PathBuf, Weak<String>>> = Mutex::new(HashMap::new());
        let path = PathBuf::from("/data/GeoLite2-City.mmdb");

        let failed = get_or_load(&cache, path.clone(), || {
            Err(GeoIpError::Other("database missing".to_string()))
        });
        assert!(failed.is_err());

        let recovered = get_or_load(&cache, path.clone(), || Ok("now present".to_string()))
            .expect("retry after a failed load should succeed");
        assert_eq!(*recovered, "now present");
    }

    #[test]
    fn test_known_hosting_orgs_detected() {
        assert!(is_hosting_org("EGIHosting"));
        assert!(is_hosting_org("Subnet Digital LLC (SDL-166)"));
        assert!(is_hosting_org("Hetzner Online GmbH"));
        assert!(is_hosting_org("Amazon.com, Inc."));
        assert!(is_hosting_org("DigitalOcean, LLC"));
        assert!(is_hosting_org("OVH SAS"));
    }

    #[test]
    fn test_residential_isp_not_flagged() {
        assert!(!is_hosting_org("Comcast Cable Communications, LLC"));
        assert!(!is_hosting_org("Verizon Communications"));
        assert!(!is_hosting_org("British Telecommunications PLC"));
    }

    #[test]
    fn test_empty_org_not_flagged() {
        assert!(!is_hosting_org(""));
        assert!(!is_hosting_org("   "));
    }

    #[test]
    fn test_case_insensitive_match() {
        assert!(is_hosting_org("SUBNET DIGITAL LLC"));
        assert!(is_hosting_org("subnet digital llc"));
    }

    #[test]
    fn mmdb_resolution_watches_data_dir_when_download_is_pending() {
        let unique = format!(
            "temps-geo-resolution-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after Unix epoch")
                .as_nanos()
        );
        let cwd = std::env::temp_dir().join(&unique).join("cwd");
        let data_dir = std::env::temp_dir().join(&unique).join("data");

        assert_eq!(
            resolve_mmdb_path_from("GeoLite2-City.mmdb", &cwd, Some(&data_dir)),
            data_dir.join("GeoLite2-City.mmdb")
        );
    }

    #[test]
    fn mmdb_resolution_keeps_existing_cwd_file_precedence() {
        let root = std::env::temp_dir().join(format!("temps-geo-existing-{}", std::process::id()));
        let cwd = root.join("cwd");
        let data_dir = root.join("data");
        std::fs::create_dir_all(&cwd).expect("create test cwd");
        let cwd_file = cwd.join("GeoLite2-City.mmdb");
        std::fs::write(&cwd_file, b"test").expect("write test mmdb placeholder");

        assert_eq!(
            resolve_mmdb_path_from("GeoLite2-City.mmdb", &cwd, Some(&data_dir)),
            cwd_file
        );

        std::fs::remove_dir_all(&root).expect("remove test directories");
    }
}
