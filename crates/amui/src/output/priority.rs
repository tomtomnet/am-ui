//! A better scheduling class for the feeder thread, so that a busy machine does not hold it off
//! the processor: `SCHED_RR | SCHED_RESET_ON_FORK` at a low priority set by the thread itself;
//! when the kernel refuses (no `RLIMIT_RTPRIO`), the same through RealtimeKit on the system bus,
//! after lowering `RLIMIT_RTTIME` to what RealtimeKit demands; when that is refused too, a lower
//! nice level through RealtimeKit; else the usual scheduling.
//!
//! The priority is below the audio server's own threads (PipeWire asks for far more), so the
//! feeder never delays them. `SCHED_RESET_ON_FORK` keeps a process started from the feeder
//! thread out of the real-time class. `RLIMIT_RTTIME` (RealtimeKit's condition) makes the kernel
//! end the process when a real-time thread runs for that long without ever blocking; the feeder
//! blocks for every block it writes.
//!
//! Everything goes through two seams: [`FeederPriority`] is what the output asks (once per
//! start, on the feeder thread), and [`SchedulerCalls`] are the kernel and D-Bus calls the
//! engine's [`SystemPriority`] makes. Tests pass their own, so that they never touch the
//! scheduler or the system bus.

use std::fmt;
use std::io;
use std::time::Duration;

use zbus::proxy::CacheProperties;

use crate::loudness::os_error_text;

/// The feeder's real-time priority: low, far below the audio server's threads.
pub const FEEDER_RT_PRIORITY: u32 = 5;
/// The nice level asked for when real-time scheduling is refused (PipeWire's own).
pub const FEEDER_NICE: i32 = -11;
/// A call to RealtimeKit that has no answer within this long has failed.
pub const REALTIME_KIT_TIMEOUT_S: u64 = 2;

const REALTIME_KIT: &str = "org.freedesktop.RealtimeKit1";
const REALTIME_KIT_PATH: &str = "/org/freedesktop/RealtimeKit1";

/// What the feeder thread got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// `SCHED_RR`, set by the thread itself.
    RoundRobin { priority: u32 },
    /// `SCHED_RR`, granted by RealtimeKit.
    RealtimeKit { priority: u32 },
    /// A lower nice level, granted by RealtimeKit (real-time scheduling was refused).
    HighPriority { nice: i32 },
    /// The usual scheduling; why each way was refused, in the order they were tried.
    Unchanged { refusals: Vec<String> },
}

impl Grant {
    /// Real-time scheduling, either way.
    pub fn realtime(&self) -> bool {
        matches!(self, Grant::RoundRobin { .. } | Grant::RealtimeKit { .. })
    }
}

impl fmt::Display for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Grant::RoundRobin { priority } => write!(f, "SCHED_RR priority {priority}, set directly"),
            Grant::RealtimeKit { priority } => write!(f, "SCHED_RR priority {priority} through RealtimeKit"),
            Grant::HighPriority { nice } => write!(f, "nice level {nice} through RealtimeKit"),
            Grant::Unchanged { refusals } if refusals.is_empty() => write!(f, "the usual scheduling"),
            Grant::Unchanged { refusals } => write!(f, "the usual scheduling ({})", refusals.join("; ")),
        }
    }
}

/// How the feeder thread asks for its scheduling class: once per `start()`, on the feeder thread,
/// before the stream is opened. The answer is logged.
pub trait FeederPriority: Send + Sync {
    fn raise(&self) -> Grant;
}

/// Leaves the scheduling as it is (tests, and outputs that must not ask).
#[derive(Debug, Default, Clone, Copy)]
pub struct KeepPriority;

impl FeederPriority for KeepPriority {
    fn raise(&self) -> Grant {
        Grant::Unchanged { refusals: vec!["not asked".to_string()] }
    }
}

/// RealtimeKit's limits, its properties of the same names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealtimeKitLimits {
    /// The highest real-time priority it hands out.
    pub max_realtime_priority: i32,
    /// The lowest nice level it hands out.
    pub min_nice_level: i32,
    /// The `RLIMIT_RTTIME` (µs) a process must not exceed to be served.
    pub rttime_usec_max: i64,
}

/// The kernel and D-Bus calls of [`raise_with`].
pub trait SchedulerCalls {
    /// The kernel's id of the calling thread.
    fn thread_id(&self) -> u64;
    /// `sched_setscheduler(this thread, SCHED_RR | SCHED_RESET_ON_FORK, priority)`.
    fn round_robin(&self, priority: u32) -> Result<(), String>;
    /// RealtimeKit's limits (it is there and answers).
    fn realtime_kit(&self) -> Result<RealtimeKitLimits, String>;
    /// Lower this process's `RLIMIT_RTTIME` (soft and hard) to at most `usec`.
    fn limit_rttime(&self, usec: u64) -> Result<(), String>;
    /// RealtimeKit's `MakeThreadRealtime(thread, priority)`.
    fn make_realtime(&self, thread: u64, priority: u32) -> Result<(), String>;
    /// RealtimeKit's `MakeThreadHighPriority(thread, nice)`.
    fn make_high_priority(&self, thread: u64, nice: i32) -> Result<(), String>;
}

/// The cascade: `SCHED_RR` directly, then through RealtimeKit (its `RLIMIT_RTTIME` first, the
/// priority within its maximum), then a nice level through RealtimeKit (within its minimum).
pub fn raise_with(calls: &dyn SchedulerCalls, priority: u32, nice: i32) -> Grant {
    let mut refusals = Vec::new();
    match calls.round_robin(priority) {
        Ok(()) => return Grant::RoundRobin { priority },
        Err(error) => refusals.push(format!("SCHED_RR: {error}")),
    }
    let limits = match calls.realtime_kit() {
        Ok(limits) => limits,
        Err(error) => {
            refusals.push(format!("RealtimeKit: {error}"));
            return Grant::Unchanged { refusals };
        }
    };
    let thread = calls.thread_id();
    let wanted = priority.min(u32::try_from(limits.max_realtime_priority).unwrap_or(0));
    if wanted == 0 {
        refusals
            .push(format!("RealtimeKit: no real-time priority to hand out (at most {})", limits.max_realtime_priority));
    } else {
        let limited = match u64::try_from(limits.rttime_usec_max) {
            Ok(usec) if usec > 0 => calls.limit_rttime(usec),
            _ => Ok(()), // no limit asked for
        };
        match limited.and_then(|()| calls.make_realtime(thread, wanted)) {
            Ok(()) => return Grant::RealtimeKit { priority: wanted },
            Err(error) => refusals.push(format!("RealtimeKit real-time: {error}")),
        }
    }
    let nice = nice.max(limits.min_nice_level);
    match calls.make_high_priority(thread, nice) {
        Ok(()) => Grant::HighPriority { nice },
        Err(error) => {
            refusals.push(format!("RealtimeKit nice level: {error}"));
            Grant::Unchanged { refusals }
        }
    }
}

/// The engine's: the kernel, then RealtimeKit on the system bus.
#[derive(Debug, Clone, Copy)]
pub struct SystemPriority {
    pub priority: u32,
    pub nice: i32,
}

impl Default for SystemPriority {
    fn default() -> SystemPriority {
        SystemPriority { priority: FEEDER_RT_PRIORITY, nice: FEEDER_NICE }
    }
}

impl FeederPriority for SystemPriority {
    fn raise(&self) -> Grant {
        raise_with(&SystemCalls, self.priority, self.nice)
    }
}

/// The real calls. Each RealtimeKit call has a system bus connection of its own, closed when
/// the call returns, so that no connection (and no thread serving one) outlives it.
struct SystemCalls;

async fn realtime_kit_proxy() -> zbus::Result<zbus::Proxy<'static>> {
    let connection =
        zbus::connection::Builder::system().method_timeout(Duration::from_secs(REALTIME_KIT_TIMEOUT_S)).build().await?;
    zbus::proxy::Builder::<zbus::Proxy<'static>>::new(&connection)
        .destination(REALTIME_KIT)
        .path(REALTIME_KIT_PATH)
        .interface(REALTIME_KIT)
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

impl SchedulerCalls for SystemCalls {
    fn thread_id(&self) -> u64 {
        // SAFETY: gettid has no preconditions.
        unsafe { libc::gettid() as u64 }
    }

    fn round_robin(&self, priority: u32) -> Result<(), String> {
        let param = libc::sched_param { sched_priority: i32::try_from(priority).unwrap_or(i32::MAX) };
        // SAFETY: 0 names the calling thread; `param` is valid for the call, which keeps nothing.
        if unsafe { libc::sched_setscheduler(0, libc::SCHED_RR | libc::SCHED_RESET_ON_FORK, &param) } == 0 {
            Ok(())
        } else {
            Err(os_error_text(&io::Error::last_os_error()))
        }
    }

    fn realtime_kit(&self) -> Result<RealtimeKitLimits, String> {
        zbus::block_on(async {
            let proxy = realtime_kit_proxy().await?;
            Ok::<_, zbus::Error>(RealtimeKitLimits {
                max_realtime_priority: proxy.get_property("MaxRealtimePriority").await?,
                min_nice_level: proxy.get_property("MinNiceLevel").await?,
                rttime_usec_max: proxy.get_property("RTTimeUSecMax").await?,
            })
        })
        .map_err(|error| error.to_string())
    }

    fn limit_rttime(&self, usec: u64) -> Result<(), String> {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: the pointer is valid for the call.
        if unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut limit) } != 0 {
            return Err(os_error_text(&io::Error::last_os_error()));
        }
        let wanted = usec as libc::rlim_t;
        if limit.rlim_cur <= wanted && limit.rlim_max <= wanted {
            return Ok(());
        }
        let lowered = libc::rlimit { rlim_cur: limit.rlim_cur.min(wanted), rlim_max: limit.rlim_max.min(wanted) };
        // SAFETY: as above; lowering a limit needs no privilege.
        if unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &lowered) } == 0 {
            Ok(())
        } else {
            Err(os_error_text(&io::Error::last_os_error()))
        }
    }

    fn make_realtime(&self, thread: u64, priority: u32) -> Result<(), String> {
        zbus::block_on(async {
            let proxy = realtime_kit_proxy().await?;
            proxy.call::<_, _, ()>("MakeThreadRealtime", &(thread, priority)).await
        })
        .map_err(|error| error.to_string())
    }

    fn make_high_priority(&self, thread: u64, nice: i32) -> Result<(), String> {
        zbus::block_on(async {
            let proxy = realtime_kit_proxy().await?;
            proxy.call::<_, _, ()>("MakeThreadHighPriority", &(thread, nice)).await
        })
        .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records the calls; answers as told.
    #[derive(Default)]
    struct Calls {
        round_robin: Option<String>,
        realtime_kit: Option<Result<RealtimeKitLimits, String>>,
        rttime: Option<String>,
        realtime: Option<String>,
        high: Option<String>,
        made: RefCell<Vec<String>>,
    }

    fn answer(refusal: &Option<String>) -> Result<(), String> {
        refusal.clone().map_or(Ok(()), Err)
    }

    impl SchedulerCalls for Calls {
        fn thread_id(&self) -> u64 {
            4321
        }

        fn round_robin(&self, priority: u32) -> Result<(), String> {
            self.made.borrow_mut().push(format!("round_robin {priority}"));
            answer(&self.round_robin)
        }

        fn realtime_kit(&self) -> Result<RealtimeKitLimits, String> {
            self.made.borrow_mut().push("realtime_kit".to_string());
            self.realtime_kit.clone().unwrap_or(Ok(RealtimeKitLimits {
                max_realtime_priority: 20,
                min_nice_level: -15,
                rttime_usec_max: 200_000,
            }))
        }

        fn limit_rttime(&self, usec: u64) -> Result<(), String> {
            self.made.borrow_mut().push(format!("limit_rttime {usec}"));
            answer(&self.rttime)
        }

        fn make_realtime(&self, thread: u64, priority: u32) -> Result<(), String> {
            self.made.borrow_mut().push(format!("make_realtime {thread} {priority}"));
            answer(&self.realtime)
        }

        fn make_high_priority(&self, thread: u64, nice: i32) -> Result<(), String> {
            self.made.borrow_mut().push(format!("make_high_priority {thread} {nice}"));
            answer(&self.high)
        }
    }

    fn refused(text: &str) -> Option<String> {
        Some(text.to_string())
    }

    #[test]
    fn the_kernel_is_asked_first() {
        let calls = Calls::default();
        assert_eq!(raise_with(&calls, 5, -11), Grant::RoundRobin { priority: 5 });
        assert_eq!(*calls.made.borrow(), ["round_robin 5"]);
    }

    #[test]
    fn realtime_kit_comes_next_with_its_time_limit_set_first() {
        let calls = Calls { round_robin: refused("[Errno 1] Operation not permitted"), ..Calls::default() };
        let grant = raise_with(&calls, 5, -11);
        assert_eq!(grant, Grant::RealtimeKit { priority: 5 });
        assert!(grant.realtime());
        let made = ["round_robin 5", "realtime_kit", "limit_rttime 200000", "make_realtime 4321 5"];
        assert_eq!(*calls.made.borrow(), made);
        // a priority above RealtimeKit's maximum is lowered to it
        let low = Calls {
            round_robin: refused("no"),
            realtime_kit: Some(Ok(RealtimeKitLimits {
                max_realtime_priority: 3,
                min_nice_level: -15,
                rttime_usec_max: 0,
            })),
            ..Calls::default()
        };
        assert_eq!(raise_with(&low, 5, -11), Grant::RealtimeKit { priority: 3 });
        assert_eq!(
            *low.made.borrow(),
            ["round_robin 5", "realtime_kit", "make_realtime 4321 3"],
            "no time limit asked for"
        );
    }

    #[test]
    fn a_nice_level_when_real_time_is_refused() {
        let calls = Calls {
            round_robin: refused("[Errno 1] Operation not permitted"),
            realtime: refused("org.freedesktop.DBus.Error.AccessDenied"),
            ..Calls::default()
        };
        assert_eq!(raise_with(&calls, 5, -11), Grant::HighPriority { nice: -11 });
        let made = [
            "round_robin 5",
            "realtime_kit",
            "limit_rttime 200000",
            "make_realtime 4321 5",
            "make_high_priority 4321 -11",
        ];
        assert_eq!(*calls.made.borrow(), made);
        // within RealtimeKit's minimum; a time limit that cannot be set is a refusal of real time
        let clamped = Calls {
            round_robin: refused("no"),
            rttime: refused("[Errno 1] Operation not permitted"),
            realtime_kit: Some(Ok(RealtimeKitLimits {
                max_realtime_priority: 20,
                min_nice_level: -5,
                rttime_usec_max: 1,
            })),
            ..Calls::default()
        };
        assert_eq!(raise_with(&clamped, 5, -11), Grant::HighPriority { nice: -5 });
        assert!(!clamped.made.borrow().iter().any(|call| call.starts_with("make_realtime")));
    }

    #[test]
    fn the_usual_scheduling_says_why() {
        let calls = Calls {
            round_robin: refused("[Errno 1] Operation not permitted"),
            realtime_kit: Some(Err("org.freedesktop.DBus.Error.ServiceUnknown".to_string())),
            ..Calls::default()
        };
        let grant = raise_with(&calls, 5, -11);
        let expected = Grant::Unchanged {
            refusals: vec![
                "SCHED_RR: [Errno 1] Operation not permitted".to_string(),
                "RealtimeKit: org.freedesktop.DBus.Error.ServiceUnknown".to_string(),
            ],
        };
        assert_eq!(grant, expected);
        assert_eq!(
            grant.to_string(),
            "the usual scheduling (SCHED_RR: [Errno 1] Operation not permitted; RealtimeKit: \
             org.freedesktop.DBus.Error.ServiceUnknown)"
        );
        assert_eq!(*calls.made.borrow(), ["round_robin 5", "realtime_kit"]);
        let everything =
            Calls { round_robin: refused("a"), realtime: refused("b"), high: refused("c"), ..Calls::default() };
        let Grant::Unchanged { refusals } = raise_with(&everything, 5, -11) else { panic!("refused") };
        assert_eq!(refusals, ["SCHED_RR: a", "RealtimeKit real-time: b", "RealtimeKit nice level: c"]);
        assert_eq!(KeepPriority.raise().to_string(), "the usual scheduling (not asked)");
        assert_eq!(Grant::RoundRobin { priority: 5 }.to_string(), "SCHED_RR priority 5, set directly");
        assert_eq!(Grant::HighPriority { nice: -11 }.to_string(), "nice level -11 through RealtimeKit");
    }
}
