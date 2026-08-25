//! Temps CLI - Single entrypoint for all services
//!
//! This binary delegates to `temps_cli::run` (defined in `lib.rs`) so the
//! same dispatch can be reused by EE-bundled binaries that need to
//! register additional plugins. See ADR 0001 §"Extension points exposed
//! by OSS".

// jemalloc fragments far less than system malloc under the proxy's workload
// and its dirty-page decay returns freed memory to the OS (system allocators
// ratchet at the high-water mark). Heap profiling: set
//   _RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19,prof_prefix:/tmp/jeprof
// (inert / zero overhead when unset). Not available on MSVC.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// jemalloc's defaults are tuned for throughput on a machine that is entirely
// the allocator's: `background_thread:false` means dirty pages are only
// purged when a thread happens to allocate in that arena again, and the
// default arena count is 4 x ncpus (96 arenas on a 24-core box). Temps runs
// its proxy, console and background workers in one binary on small machines
// (the reference deployment is 3 vCPU / 4 GB), where an arena that goes idle
// after a traffic burst holds onto its dirty pages indefinitely and the
// process RSS ratchets at the high-water mark.
//
// Enabling the background thread makes purging time-driven instead of
// allocation-driven, and shortening both decay windows from the 10s dirty /
// 10s muzzy defaults to 5s returns physical pages to the kernel promptly
// after a burst. The cost is bounded: one extra thread per arena set, and
// pages that are re-faulted if the same arena allocates again shortly after
// a purge — cheap relative to carrying the peak footprint forever.
//
// `MALLOC_CONF` / `_RJEM_MALLOC_CONF` from the environment is read *after*
// this symbol, so the heap-profiling override documented above still wins.
#[cfg(not(target_env = "msvc"))]
#[allow(non_upper_case_globals)]
#[export_name = "_rjem_malloc_conf"]
pub static MALLOC_CONF: &[u8] = b"background_thread:true,dirty_decay_ms:5000,muzzy_decay_ms:5000\0";

fn main() -> anyhow::Result<()> {
    temps_cli::run(Vec::new())
}
