//! Device connection logic: discovery, mount/eject command construction,
//! sudo interaction and error classification.
//!
//! Pure logic, no egui, so everything is testable without a display.
//! Command *construction* returns inert [`CommandSpec`] values (plain
//! strings, never executed); only the `run_*` helpers execute processes,
//! keeping the exact command lines unit-testable.

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// An inert command: program + args as plain strings. Built by the pure
/// constructors ([`build_mount_command`], [`build_shutdown_steps`]) so
/// tests can assert exact command lines without executing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    pub(crate) fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }
}

/// A discovered device candidate: either an already-mounted volume
/// directory that contains an `iPod_Control` entry, or a whole Apple
/// disk found in sysfs while unmounted (machines without an automount
/// daemon — the GUI mounts it itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateKind {
    /// Volume is mounted; `mount` is the actual mount point.
    Mounted,
    /// Volume is not mounted; `mount` is the *planned* mount point
    /// (see [`default_mount_path`]) that «Подключить» will create.
    Unmounted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCandidate {
    /// Mounted: actual mount point. Unmounted: planned mount point.
    pub mount: PathBuf,
    /// Volume directory name (UUID or label), for display.
    pub label: String,
    pub kind: CandidateKind,
}

/// Scans the system media root for device candidates.
pub fn scan_candidates() -> Vec<DeviceCandidate> {
    scan_under(Path::new("/run/media"))
}

/// Pure scan core: every directory two levels below `root` that contains
/// an `iPod_Control` subdirectory is a candidate. Results are sorted by
/// mount path for determinism (tests rely on it).
pub fn scan_under(root: &Path) -> Vec<DeviceCandidate> {
    let mut candidates = Vec::new();
    let Ok(users) = std::fs::read_dir(root) else {
        return candidates;
    };
    for user in users.flatten() {
        let Ok(volumes) = std::fs::read_dir(user.path()) else {
            continue;
        };
        for volume in volumes.flatten() {
            if volume.path().join("iPod_Control").is_dir() {
                candidates.push(DeviceCandidate {
                    label: volume.file_name().to_string_lossy().into_owned(),
                    mount: volume.path(),
                    kind: CandidateKind::Mounted,
                });
            }
        }
    }
    candidates.sort_by(|a, b| a.mount.cmp(&b.mount));
    candidates
}

/// An Apple iPod detected through sysfs while its data volume is not
/// mounted anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppleDisk {
    /// The largest partition (the data volume; the tiny first partition
    /// is an MBR/GPT stub), e.g. `/dev/sda2`.
    pub partition: PathBuf,
    /// `vendor model`, e.g. `Apple iPod`, for display and mount label.
    pub label: String,
}

/// Pure scan core with an injectable sysfs root: every `/sys/block/*`
/// entry whose `device/vendor` is `Apple` is an iPod. Its data partition
/// is the largest partition child (children carrying the `partition`
/// marker file, sized by the `size` file in 512-byte sectors). The
/// devnode name is the basename of the sysfs partition dir (`sda2` →
/// `/dev/sda2`) — safe because the vendor filter admits only sd-like
/// USB disks (WHY: no libudev, KISS per the round-1 decision).
pub fn scan_apple_disks(sys_block_root: &Path) -> Vec<AppleDisk> {
    let mut disks = Vec::new();
    let Ok(entries) = std::fs::read_dir(sys_block_root) else {
        return disks;
    };
    for entry in entries.flatten() {
        let sys_text = |rel: &str| -> String {
            std::fs::read_to_string(entry.path().join(rel))
                .map(|text| text.trim().to_owned())
                .unwrap_or_default()
        };
        let vendor = sys_text("device/vendor");
        if vendor != "Apple" {
            continue;
        }
        let model = sys_text("device/model");
        let Ok(parts) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        let data = parts
            .flatten()
            .filter(|part| part.path().join("partition").is_file())
            .filter_map(|part| {
                let size: u64 = std::fs::read_to_string(part.path().join("size"))
                    .ok()?
                    .trim()
                    .parse()
                    .ok()?;
                let name = part.file_name().to_string_lossy().into_owned();
                Some((size, name))
            })
            .max_by_key(|(size, _)| *size);
        let Some((_, part_name)) = data else {
            continue;
        };
        let label = if model.is_empty() {
            vendor
        } else {
            format!("{vendor} {model}")
        };
        disks.push(AppleDisk {
            partition: PathBuf::from("/dev").join(part_name),
            label,
        });
    }
    disks.sort_by(|a, b| a.partition.cmp(&b.partition));
    disks
}

/// Planned mount point for an unmounted iPod, following the udisks
/// convention `/run/media/<user>/<label>` so the layout matches
/// automounted volumes on machines that have an automount daemon.
pub fn default_mount_path(label: &str, user: &str) -> PathBuf {
    Path::new("/run/media").join(user).join(label)
}

/// Pure merge: mounted candidates win (dedup by block device — a sysfs
/// disk whose data partition is already mounted is the same iPod);
/// unmounted disks become candidates with a planned mount path. Output
/// is sorted by mount path for determinism (tests rely on it).
pub fn merge_candidates(
    mounted: Vec<(DeviceCandidate, Option<PathBuf>)>,
    disks: Vec<AppleDisk>,
    user: &str,
) -> Vec<(DeviceCandidate, Option<PathBuf>)> {
    let mut out: Vec<(DeviceCandidate, Option<PathBuf>)> = Vec::new();
    let mut seen_devs: Vec<Option<PathBuf>> = Vec::new();
    for (candidate, dev) in mounted {
        // Stacked mounts of one devnode (repeated «Подключить» over an
        // existing mount) collapse into one row: the first target wins.
        // A mounted candidate without a resolvable devnode is a stale
        // mount of an unplugged device — shown as Unmounted instead of
        // crashing or pretending it is usable.
        if seen_devs.contains(&dev) {
            continue;
        }
        seen_devs.push(dev.clone());
        let candidate = match candidate.kind {
            CandidateKind::Mounted if dev.is_none() => DeviceCandidate {
                mount: default_mount_path(&candidate.label, user),
                label: candidate.label,
                kind: CandidateKind::Unmounted,
            },
            kind_state => DeviceCandidate {
                mount: candidate.mount,
                label: candidate.label,
                kind: kind_state,
            },
        };
        out.push((candidate, dev));
    }
    for disk in disks {
        if seen_devs.iter().any(|dev| dev.as_deref() == Some(disk.partition.as_path())) {
            continue;
        }
        seen_devs.push(Some(disk.partition.clone()));
        out.push((
            DeviceCandidate {
                mount: default_mount_path(&disk.label, user),
                label: disk.label,
                kind: CandidateKind::Unmounted,
            },
            Some(disk.partition),
        ));
    }
    out.sort_by(|a, b| a.0.mount.cmp(&b.0.mount));
    out
}

/// Parsed `/proc/mounts` entry: (source device, mount point), with the
/// kernel's octal escapes (`\040` space, `\011` tab, `\012` newline,
/// `\134` backslash) decoded.
pub fn parse_proc_mounts(content: &str) -> Vec<(String, String)> {
    content
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?;
            let target = fields.next()?;
            Some((unescape_mount(source), unescape_mount(target)))
        })
        .collect()
}

/// Decodes the kernel's octal escapes in `/proc/mounts` fields.
fn unescape_mount(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = bytes[index] == b'\\'
            && index + 4 <= bytes.len()
            && bytes[index + 1..index + 4].iter().all(u8::is_ascii_digit);
        if escape {
            let byte = u8::from_str_radix(&text[index + 1..index + 4], 8).unwrap_or(b'\\');
            out.push(byte);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Mount-time decision for a planned (source, target) pair: the target
/// may already carry the same source (skip the mount — idempotent), a
/// different one (honest refusal), or be free.
pub enum MountDecision {
    AlreadyMounted,
    ConflictsWith(String),
    Free,
}

pub fn decide_mount(mounts: &[(String, String)], source: &Path, target: &Path) -> MountDecision {
    let target = target.display().to_string();
    let source = source.display().to_string();
    for (mounted_source, mounted_target) in mounts {
        if mounted_target == &target {
            return if mounted_source == &source {
                MountDecision::AlreadyMounted
            } else {
                MountDecision::ConflictsWith(mounted_source.clone())
            };
        }
    }
    MountDecision::Free
}

/// How many mounts of `source` the parsed `/proc/mounts` snapshot
/// carries: each stacked layer needs its own unmount during the eject
/// drain (the live incident had seven layers on one mount point, where
/// a single unmount removed exactly one layer).
pub fn count_mounts_with_source(mounts: &[(String, String)], source: &Path) -> usize {
    let source = source.display().to_string();
    mounts
        .iter()
        .filter(|(mounted_source, _)| *mounted_source == source)
        .count()
}

/// Full discovery, the «Обновить» button body: mounted iPod volumes plus
/// unmounted Apple disks from sysfs. Returns each candidate with its
/// resolved block device (resolved for mounted volumes, known from sysfs
/// for unmounted disks).
pub fn discover_devices() -> Vec<(DeviceCandidate, Option<PathBuf>)> {
    let mounted = scan_candidates()
        .into_iter()
        .map(|candidate| {
            let dev = resolve_block_device(&candidate.mount);
            (candidate, dev)
        })
        .collect::<Vec<_>>();
    let disks = scan_apple_disks(Path::new("/sys/block"));
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_owned());
    merge_candidates(mounted, disks, &user)
}

/// Resolves the block device backing a mount point via sysfs: stat the
/// mount, decode dev_t to `major:minor`, then map `/sys/dev/block/M:N`
/// (a symlink named after the device, e.g. `sda2`) to `/dev/sda2`.
/// No libudev — KISS per the round-1 decision.
pub fn resolve_block_device(mount: &Path) -> Option<PathBuf> {
    let dev = std::fs::metadata(mount).ok()?.dev();
    block_device_from_dev(dev, Path::new("/sys/dev/block"))
}

/// Pure core of [`resolve_block_device`] with an injectable sysfs root,
/// so tests can build a fake `/sys/dev/block`.
pub fn block_device_from_dev(dev: u64, sys_block_dir: &Path) -> Option<PathBuf> {
    let id = format!("{}:{}", dev_major(dev), dev_minor(dev));
    let link = std::fs::read_link(sys_block_dir.join(id)).ok()?;
    let name = link.file_name()?; // `…/block/sda/sda2` → `sda2`
    Some(PathBuf::from("/dev").join(name))
}

/// Real uid/gid of this process. std has no `getuid()`; on Linux
/// `/proc/self` is owned by the caller, so stat(2) yields both values —
/// no libc dependency needed.
pub fn current_uid_gid() -> Option<(u32, u32)> {
    let meta = std::fs::metadata("/proc/self").ok()?;
    Some((meta.uid(), meta.gid()))
}

/// glibc dev_t encoding (the kernel exports the same userland encoding
/// for device numbers since 2.6.26).
fn dev_major(dev: u64) -> u32 {
    (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)) as u32
}

fn dev_minor(dev: u64) -> u32 {
    ((dev & 0xff) | ((dev >> 12) & !0xff)) as u32
}

/// Whole-disk path for a block device, resolved via sysfs (no string
/// parsing of device names). A device is a partition iff its sysfs entry
/// contains the `partition` marker file (`/sys/dev/block/M:N/partition`);
/// the parent disk is then the name of the resolved entry's parent
/// directory (`…/block/sda/sda2` → `sda` → `/dev/sda`). Without the
/// marker the device is already a whole disk — `sda`, but also `loop0`,
/// `dm-0`, `zram0`, which name stripping used to mangle — and is
/// returned unchanged. The same fallback applies when sysfs has no entry
/// for the device (stat failure or unknown dev_t).
pub fn parent_disk(block_dev: &Path, sys_block_dir: &Path) -> PathBuf {
    let Ok(meta) = std::fs::metadata(block_dev) else {
        return block_dev.to_owned();
    };
    let id = format!("{}:{}", dev_major(meta.dev()), dev_minor(meta.dev()));
    let entry = sys_block_dir.join(id); // symlink to the device directory
    let Ok(link) = std::fs::read_link(&entry) else {
        return block_dev.to_owned();
    };
    // The marker lives inside the device directory, i.e. behind the symlink.
    if !entry.join("partition").exists() {
        return block_dev.to_owned();
    }
    // Parent directory of the resolved entry: `…/block/sda/sda2` → `…/block/sda`.
    let resolved = if link.is_absolute() {
        link
    } else {
        sys_block_dir.join(link)
    };
    match resolved.parent().and_then(Path::file_name) {
        Some(disk) => PathBuf::from("/dev").join(disk),
        None => block_dev.to_owned(),
    }
}

/// Everything needed to rw-mount an HFS+ iPod volume for the current user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    pub block_dev: PathBuf,
    pub mount_path: PathBuf,
    pub uid: u32,
    pub gid: u32,
}

/// rw-mount recipe from `docs/device-ipod-nano-7g.md`:
/// `mount -t hfsplus -o force,uid=…,gid=…,umask=22,nls=utf8 <dev> <mount>`.
/// `force` overrides udisks' read-only policy, uid/gid/umask make the
/// files owned by the current user, `nls=utf8` keeps non-ASCII names.
pub fn build_mount_command(spec: &MountSpec) -> CommandSpec {
    CommandSpec {
        program: "mount".to_owned(),
        args: vec![
            "-t".to_owned(),
            "hfsplus".to_owned(),
            "-o".to_owned(),
            format!("force,uid={},gid={},umask=22,nls=utf8", spec.uid, spec.gid),
            spec.block_dev.display().to_string(),
            spec.mount_path.display().to_string(),
        ],
    }
}

/// Connect sequence for a possibly-missing mount point: `mkdir -p`
/// first (`/run/media/<user>` is root-owned, so an unmounted iPod has
/// no directory yet), then the rw-mount. `mkdir -p` on an existing
/// directory is a no-op, so the same sequence serves the already-mounted
/// flow too — one code path in the connect worker.
pub fn build_mount_sequence(spec: &MountSpec) -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("mkdir", &["-p", &spec.mount_path.display().to_string()]),
        build_mount_command(spec),
    ]
}

/// Safe-eject shutdown plan (round 6.1): `sync` → one `unmount` per
/// stacked mount of the volume → `power-off` LAST. The order is
/// load-bearing: power-off with any mount still alive fails with
/// «The drive is in use», which is exactly how the live eject died
/// (the old drain ran after power-off, behind a `?`, unreachable).
/// Returns the ordered steps (sync first) and the power-off step
/// separately. Round 6.2: ALL of them run under sudo (see
/// [`build_sudo_command`], one password for the whole shutdown) — the
/// volume is mounted by us as root, so a user-side unmount would
/// trigger a polkit dialog (a second password prompt). Power-off stays
/// strictly last; no polkit agent needed.
/// `mounts` is the parsed `/proc/mounts` snapshot; the drain is capped
/// at [`EJECT_DRAIN_LIMIT`] layers. `sys_block_dir` is the injectable
/// sysfs root used to resolve the whole-disk device — see
/// [`parent_disk`].
pub fn build_shutdown_steps(
    block_dev: &Path,
    sys_block_dir: &Path,
    mounts: &[(String, String)],
) -> (Vec<CommandSpec>, CommandSpec) {
    let layers = count_mounts_with_source(mounts, block_dev).min(EJECT_DRAIN_LIMIT);
    let mut steps = vec![CommandSpec::new("sync", &[])];
    steps.extend(std::iter::repeat_n(
        CommandSpec::new("udisksctl", &["unmount", "-b", &block_dev.display().to_string()]),
        layers,
    ));
    let power_off = CommandSpec::new(
        "udisksctl",
        &[
            "power-off",
            "-b",
            &parent_disk(block_dev, sys_block_dir).display().to_string(),
        ],
    );
    (steps, power_off)
}

/// Drain cap of the eject shutdown: stacked-mount layers are bounded so
/// a runaway mount table cannot spin the unmount sequence forever.
pub const EJECT_DRAIN_LIMIT: usize = 10;

/// Human-readable name of one shutdown step for error messages
/// («шаг unmount (sudo): …»). The `udisksctl` subcommands are told
/// apart by argv — both run the same program.
pub fn shutdown_step_label(spec: &CommandSpec) -> &'static str {
    match (spec.program.as_str(), spec.args.first().map(String::as_str)) {
        ("udisksctl", Some("power-off")) => "power-off",
        ("udisksctl", _) => "unmount",
        _ => "sync",
    }
}

/// Uniform UI-facing failure of one sudo shutdown step: the step name
/// plus the tool's own stderr (or the spawn error text).
pub fn shutdown_step_error(spec: &CommandSpec, stderr: &str) -> String {
    format!("шаг {} (sudo): {}", shutdown_step_label(spec), stderr.trim())
}

/// Wraps `spec` in `sudo -S -p ''`: the password is read from stdin and
/// the prompt is empty, keeping stderr clean for error classification.
/// The password never reaches argv, so it cannot leak via process lists.
pub fn build_sudo_command(spec: &CommandSpec) -> Command {
    let mut command = Command::new("sudo");
    command
        .arg("-S") // read password from stdin
        .arg("-p")
        .arg("") // empty prompt
        .arg(&spec.program)
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Runs `spec` under sudo, feeding the password over the stdin pipe.
/// Shared by every privileged system action: the connect mount
/// sequence and the whole eject shutdown (round 6.2). The password
/// lives only in the child's memory for the duration of the command;
/// it is never logged, persisted, or placed in argv.
pub fn run_sudo_command(password: &str, spec: &CommandSpec) -> std::io::Result<Output> {
    let mut child = build_sudo_command(spec).spawn()?;
    {
        // `build_sudo_command` always pipes stdin; `None` here would mean
        // an internal bug, so fail with an error instead of panicking.
        let stdin = child.stdin.as_mut().ok_or_else(|| {
            std::io::Error::other("stdin не открыт: build_sudo_command не настроил pipe")
        })?;
        // The write is best-effort (`let _ =`): in a loop of
        // back-to-back sudo steps the already-validated password is
        // cached (sudo timestamp), so a later sudo may exit WITHOUT
        // reading stdin — our write then fails with EPIPE, which is NOT
        // a command failure. Success/failure is decided solely by the
        // exit status checked by the caller.
        let _ = stdin.write_all(password.as_bytes());
        let _ = stdin.write_all(b"\n");
    } // stdin dropped → pipe closed → sudo sees EOF
    child.wait_with_output()
}

/// Failure kinds of the mount step, classified from sudo/mount stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountFailure {
    WrongPassword,
    DirtyJournal,
    AlreadyMounted,
    Other,
}

/// Classifies mount stderr. Markers follow the runbook: sudo auth
/// failures, dirty HFS+ journal errors («journal is not empty»,
/// «uninitialized node») and busy/already-mounted states.
pub fn classify_mount_failure(stderr: &str) -> MountFailure {
    let stderr = stderr.to_lowercase();
    const PASSWORD_MARKERS: [&str; 4] = [
        "sorry, try again",
        "incorrect password",
        "no password was provided",
        "authentication failure",
    ];
    if PASSWORD_MARKERS.iter().any(|marker| stderr.contains(marker)) {
        return MountFailure::WrongPassword;
    }
    if stderr.contains("journal") || stderr.contains("uninitialized node") {
        return MountFailure::DirtyJournal;
    }
    if stderr.contains("already mounted")
        || stderr.contains("is busy")
        || stderr.contains("resource busy")
    {
        return MountFailure::AlreadyMounted;
    }
    MountFailure::Other
}

/// Classified mount errors with UI-facing hints.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("sudo отклонил пароль")]
    WrongPassword,
    #[error("журнал HFS+ грязный — force-монтирование невозможно")]
    DirtyJournal,
    #[error("том уже смонтирован")]
    AlreadyMounted,
    #[error("mount завершился с ошибкой")]
    Failed,
    #[error("не удалось выполнить sudo: {0}")]
    Io(#[from] std::io::Error),
}

impl MountError {
    /// Extra advice beyond the error message itself, if any.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            MountError::WrongPassword => {
                Some("Проверьте пароль sudo (раскладка, CapsLock).")
            }
            MountError::DirtyJournal => Some(
                "Журнал HFS+ грязный (обычно после прерванного синка). \
                 Выполните: fsck.hfsplus -f <блочное устройство> — \
                 см. docs/device-ipod-nano-7g.md.",
            ),
            MountError::AlreadyMounted => {
                Some("Том уже смонтирован; при необходимости отмонтируйте и повторите.")
            }
            MountError::Failed | MountError::Io(_) => None,
        }
    }
}

/// Classifies a finished mount command: exit status + stderr kind.
pub fn classify_mount_output(output: &Output) -> Result<(), MountError> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(match classify_mount_failure(&stderr) {
        MountFailure::WrongPassword => MountError::WrongPassword,
        MountFailure::DirtyJournal => MountError::DirtyJournal,
        MountFailure::AlreadyMounted => MountError::AlreadyMounted,
        MountFailure::Other => MountError::Failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use tempfile::TempDir;

    fn touch_dir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
    }

    #[test]
    fn scan_under_finds_ipod_volumes() {
        let root = TempDir::new().unwrap();
        touch_dir(&root.path().join("user1/uuid-abc/iPod_Control"));
        touch_dir(&root.path().join("user1/plain-volume"));
        touch_dir(&root.path().join("user2/uuid-def/iPod_Control"));
        let found = scan_under(root.path());
        assert_eq!(
            found,
            vec![
                DeviceCandidate {
                    mount: root.path().join("user1/uuid-abc"),
                    label: "uuid-abc".to_owned(),
                    kind: CandidateKind::Mounted,
                },
                DeviceCandidate {
                    mount: root.path().join("user2/uuid-def"),
                    label: "uuid-def".to_owned(),
                    kind: CandidateKind::Mounted,
                },
            ]
        );
    }

    /// Fake sysfs disk: `sys/block/<name>` with `device/{vendor,model}`
    /// and partition children carrying the `partition` marker + `size`.
    fn fake_apple_disk(root: &Path, name: &str, partitions: &[(&str, u64)]) {
        let disk = root.join(name);
        touch_dir(&disk.join("device"));
        std::fs::write(disk.join("device/vendor"), b"Apple\n").unwrap();
        std::fs::write(disk.join("device/model"), b"iPod\n").unwrap();
        for (part, size) in partitions {
            let dir = disk.join(part);
            touch_dir(&dir);
            std::fs::write(dir.join("partition"), b"2\n").unwrap();
            std::fs::write(dir.join("size"), size.to_string()).unwrap();
        }
    }

    #[test]
    fn apple_disk_scan_picks_the_largest_partition() {
        let sys = TempDir::new().unwrap();
        fake_apple_disk(
            sys.path(),
            "sda",
            &[("sda1", 488), ("sda2", 30_800_368)],
        );
        let disks = scan_apple_disks(sys.path());
        assert_eq!(
            disks,
            vec![AppleDisk {
                partition: PathBuf::from("/dev/sda2"),
                label: "Apple iPod".to_owned(),
            }]
        );
    }

    #[test]
    fn apple_disk_scan_ignores_other_vendors_and_partitionless_disks() {
        let sys = TempDir::new().unwrap();
        let ata = sys.path().join("nvme0n1");
        touch_dir(&ata.join("device"));
        std::fs::write(ata.join("device/vendor"), b"ATA\n").unwrap();
        touch_dir(&sys.path().join("sdb/device")); // Apple, no partitions yet
        std::fs::write(sys.path().join("sdb/device/vendor"), b"Apple\n").unwrap();
        assert!(scan_apple_disks(sys.path()).is_empty());
    }

    #[test]
    fn merge_drops_disk_when_its_partition_is_already_mounted() {
        let mounted = vec![(
            DeviceCandidate {
                mount: PathBuf::from("/run/media/user/uuid"),
                label: "uuid".to_owned(),
                kind: CandidateKind::Mounted,
            },
            Some(PathBuf::from("/dev/sda2")),
        )];
        let disks = vec![AppleDisk {
            partition: PathBuf::from("/dev/sda2"),
            label: "Apple iPod".to_owned(),
        }];
        let merged = merge_candidates(mounted, disks, "user");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].0.kind, CandidateKind::Mounted);
    }

    #[test]
    fn merge_adds_unmounted_disk_with_planned_mount_path() {
        let disks = vec![AppleDisk {
            partition: PathBuf::from("/dev/sda2"),
            label: "Apple iPod".to_owned(),
        }];
        let merged = merge_candidates(Vec::new(), disks, "user");
        assert_eq!(merged.len(), 1);
        let (candidate, dev) = &merged[0];
        assert_eq!(candidate.kind, CandidateKind::Unmounted);
        assert_eq!(candidate.mount, PathBuf::from("/run/media/user/Apple iPod"));
        assert_eq!(candidate.label, "Apple iPod");
        // The partition comes straight from sysfs — no mount stat needed.
        assert_eq!(dev.as_deref(), Some(Path::new("/dev/sda2")));
    }

    #[test]
    fn merge_dedupes_stacked_mounts_of_one_devnode() {
        // Round 5.3: repeated connects stack mounts of the same sda2;
        // the sidebar must show ONE row (first target), not N.
        let mounted = vec![
            (
                DeviceCandidate {
                    mount: PathBuf::from("/run/media/user/iPod"),
                    label: "iPod".to_owned(),
                    kind: CandidateKind::Mounted,
                },
                Some(PathBuf::from("/dev/sda2")),
            ),
            (
                DeviceCandidate {
                    mount: PathBuf::from("/run/media/user/iPod1"),
                    label: "iPod1".to_owned(),
                    kind: CandidateKind::Mounted,
                },
                Some(PathBuf::from("/dev/sda2")),
            ),
        ];
        let disks = vec![AppleDisk {
            partition: PathBuf::from("/dev/sda2"),
            label: "Apple iPod".to_owned(),
        }];
        let merged = merge_candidates(mounted, disks, "user");
        assert_eq!(merged.len(), 1, "one devnode — one candidate");
        assert_eq!(merged[0].0.kind, CandidateKind::Mounted);
        assert_eq!(merged[0].0.mount, PathBuf::from("/run/media/user/iPod"));
        assert_eq!(merged[0].1.as_deref(), Some(Path::new("/dev/sda2")));
    }

    #[test]
    fn stale_mount_without_devnode_shows_as_unmounted() {
        let mounted = vec![(
            DeviceCandidate {
                mount: PathBuf::from("/run/media/user/iPod"),
                label: "iPod".to_owned(),
                kind: CandidateKind::Mounted,
            },
            None,
        )];
        let merged = merge_candidates(mounted, Vec::new(), "user");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].0.kind, CandidateKind::Unmounted);
        assert_eq!(
            merged[0].0.mount,
            PathBuf::from("/run/media/user/iPod"),
            "planned mount path"
        );
        assert_eq!(merged[0].1, None);
    }

    #[test]
    fn proc_mounts_parses_fields_and_octal_escapes() {
        let mounts = parse_proc_mounts(
            "/dev/sda2 /run/media/user/iPod hfsplus ro 0 0\n"
                .replace("\\n", "\n")
                .as_str(),
        );
        assert_eq!(
            mounts,
            vec![(
                "/dev/sda2".to_owned(),
                "/run/media/user/iPod".to_owned(),
            )
        ]);
        let escaped = "/dev/sda\\0402 /run/media/user/iPod\\0402 hfsplus ro 0 0\\n";
        let mounts = parse_proc_mounts(escaped);
        assert_eq!(
            mounts,
            vec![(
                "/dev/sda 2".to_owned(),
                "/run/media/user/iPod 2".to_owned(),
            )
        ]);
    }

    #[test]
    fn mount_decision_is_idempotent_or_honest() {
        let mounts = vec![
            (
                "/dev/sda2".to_owned(),
                "/run/media/user/iPod".to_owned(),
            ),
            (
                "/dev/sdb1".to_owned(),
                "/run/media/user/other".to_owned(),
            ),
        ];
        let source = Path::new("/dev/sda2");
        let target = Path::new("/run/media/user/iPod");
        assert!(matches!(
            decide_mount(&mounts, source, target),
            MountDecision::AlreadyMounted
        ));
        assert!(matches!(
            decide_mount(&mounts, Path::new("/dev/sdc1"), target),
            MountDecision::ConflictsWith(ref other) if other == "/dev/sda2"
        ));
        assert!(matches!(
            decide_mount(&mounts, Path::new("/dev/sda2"), Path::new("/run/media/user/new")),
            MountDecision::Free
        ));
    }

    #[test]
    fn count_mounts_with_source_counts_stacked_layers() {
        let mounts = vec![
            ("/dev/sda2".to_owned(), "/run/media/user/iPod".to_owned()),
            ("/dev/sda2".to_owned(), "/run/media/user/iPod1".to_owned()),
            ("/dev/sdb1".to_owned(), "/run/media/user/other".to_owned()),
        ];
        assert_eq!(count_mounts_with_source(&mounts, Path::new("/dev/sda2")), 2);
        assert_eq!(count_mounts_with_source(&mounts, Path::new("/dev/sdb1")), 1);
        assert_eq!(count_mounts_with_source(&[], Path::new("/dev/sda2")), 0);
        assert_eq!(count_mounts_with_source(&mounts, Path::new("/dev/sda1")), 0);
    }

    #[test]
    fn mount_sequence_is_mkdir_then_mount() {
        let spec = MountSpec {
            block_dev: PathBuf::from("/dev/sda2"),
            mount_path: PathBuf::from("/run/media/user/Apple iPod"),
            uid: 1000,
            gid: 1000,
        };
        let steps = build_mount_sequence(&spec);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].program, "mkdir");
        assert_eq!(
            steps[0].args,
            vec![
                "-p".to_owned(),
                "/run/media/user/Apple iPod".to_owned(),
            ]
        );
        assert_eq!(steps[1], build_mount_command(&spec));
    }

    #[test]
    fn scan_under_missing_root_is_empty() {
        assert!(scan_under(Path::new("/nonexistent/media/root")).is_empty());
    }

    #[test]
    fn ipod_control_must_be_a_directory() {
        let root = TempDir::new().unwrap();
        touch_dir(&root.path().join("user/vol"));
        std::fs::write(root.path().join("user/vol/iPod_Control"), b"file").unwrap();
        assert!(scan_under(root.path()).is_empty());
    }

    #[test]
    fn dev_number_decodes_major_and_minor() {
        // (8, 2) → sda2; (259, 3) → nvme0n1p3-style numbering.
        let sys = TempDir::new().unwrap();
        let block_dir = sys.path().join("dev/block");
        touch_dir(&block_dir);
        std::os::unix::fs::symlink(
            "../../devices/platform/usb/block/sda/sda2",
            block_dir.join("8:2"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../../devices/pci/block/nvme0n1/nvme0n1p3",
            block_dir.join("259:3"),
        )
        .unwrap();
        assert_eq!(
            block_device_from_dev((8 << 8) | 2, &block_dir),
            Some(PathBuf::from("/dev/sda2"))
        );
        assert_eq!(
            block_device_from_dev((259 << 8) | 3, &block_dir),
            Some(PathBuf::from("/dev/nvme0n1p3"))
        );
    }

    #[test]
    fn block_device_unknown_when_sysfs_lacks_entry() {
        let sys = TempDir::new().unwrap();
        assert_eq!(
            block_device_from_dev((8 << 8) | 5, &sys.path().join("dev/block")),
            None
        );
    }

    /// Builds a fake sysfs entry: a fake `/dev` node (only its dev_t is
    /// read), a `sys/dev/block/M:N` symlink pointing at `link_target`,
    /// and — when `is_partition` — the `partition` marker inside the
    /// target directory. Returns the device path and the fake sysfs
    /// block dir, ready for [`parent_disk`]/[`build_unmount_sequence`].
    fn fake_block_device(root: &Path, link_target: &str, is_partition: bool) -> (PathBuf, PathBuf) {
        let dev_node = root.join("dev-node");
        std::fs::write(&dev_node, b"").unwrap();
        let meta = std::fs::metadata(&dev_node).unwrap();
        let id = format!("{}:{}", dev_major(meta.dev()), dev_minor(meta.dev()));
        let sys_block_dir = root.join("sys/dev/block");
        touch_dir(&sys_block_dir);
        std::os::unix::fs::symlink(link_target, sys_block_dir.join(&id)).unwrap();
        if is_partition {
            // The marker lives in the target directory; create_dir_all
            // resolves the `..` segments of the relative link target.
            let target_dir = sys_block_dir.join(link_target);
            std::fs::create_dir_all(&target_dir).unwrap();
            std::fs::write(target_dir.join("partition"), b"").unwrap();
        }
        (dev_node, sys_block_dir)
    }

    #[test]
    fn parent_disk_partition_resolves_parent_via_sysfs() {
        let sys = TempDir::new().unwrap();
        let (dev, sys_block_dir) = fake_block_device(
            sys.path(),
            "../../devices/platform/usb/block/sda/sda2",
            true,
        );
        assert_eq!(parent_disk(&dev, &sys_block_dir), PathBuf::from("/dev/sda"));
    }

    #[test]
    fn parent_disk_nvme_partition_resolves_parent_via_sysfs() {
        let sys = TempDir::new().unwrap();
        let (dev, sys_block_dir) = fake_block_device(
            sys.path(),
            "../../devices/pci0000:00/block/nvme0n1/nvme0n1p2",
            true,
        );
        assert_eq!(
            parent_disk(&dev, &sys_block_dir),
            PathBuf::from("/dev/nvme0n1")
        );
    }

    #[test]
    fn parent_disk_whole_disk_without_marker_stays_itself() {
        let sys = TempDir::new().unwrap();
        let (dev, sys_block_dir) = fake_block_device(
            sys.path(),
            "../../devices/platform/usb/block/sda/sda",
            false,
        );
        assert_eq!(parent_disk(&dev, &sys_block_dir), dev);
    }

    #[test]
    fn parent_disk_virtual_devices_without_marker_stay_themselves() {
        // loop0/dm-0/zram0 have sysfs entries but no `partition` marker;
        // naive digit stripping used to invent /dev/loop, /dev/dm, /dev/zram.
        for name in ["loop0", "dm-0", "zram0"] {
            let sys = TempDir::new().unwrap();
            let target = format!("../../devices/platform/virtual/block/{name}");
            let (dev, sys_block_dir) = fake_block_device(sys.path(), &target, false);
            assert_eq!(parent_disk(&dev, &sys_block_dir), dev, "case: {name}");
        }
    }

    #[test]
    fn parent_disk_unknown_sysfs_entry_falls_back_to_itself() {
        let sys = TempDir::new().unwrap();
        let dev_node = sys.path().join("dev-node");
        std::fs::write(&dev_node, b"").unwrap();
        let sys_block_dir = sys.path().join("sys/dev/block");
        touch_dir(&sys_block_dir);
        assert_eq!(parent_disk(&dev_node, &sys_block_dir), dev_node);
    }

    #[test]
    fn mount_command_matches_runbook_recipe() {
        let spec = MountSpec {
            block_dev: PathBuf::from("/dev/sda2"),
            mount_path: PathBuf::from("/run/media/user/uuid"),
            uid: 1000,
            gid: 1000,
        };
        let command = build_mount_command(&spec);
        assert_eq!(command.program, "mount");
        assert_eq!(
            command.args,
            vec![
                "-t".to_owned(),
                "hfsplus".to_owned(),
                "-o".to_owned(),
                "force,uid=1000,gid=1000,umask=22,nls=utf8".to_owned(),
                "/dev/sda2".to_owned(),
                "/run/media/user/uuid".to_owned(),
            ]
        );
    }

    #[test]
    fn sudo_command_hides_password_in_stdin_not_argv() {
        let spec = build_mount_command(&MountSpec {
            block_dev: PathBuf::from("/dev/sda2"),
            mount_path: PathBuf::from("/run/media/user/uuid"),
            uid: 1000,
            gid: 1000,
        });
        let sudo = build_sudo_command(&spec);
        assert_eq!(sudo.get_program().to_string_lossy(), "sudo");
        let args: Vec<_> = sudo
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..3], &["-S", "-p", ""]);
        assert_eq!(&args[3], &spec.program);
        assert_eq!(&args[4..], spec.args.as_slice());
    }

    #[test]
    fn shutdown_steps_are_sync_drain_then_power_off_last() {
        let sys = TempDir::new().unwrap();
        let (dev, sys_block_dir) =
            fake_block_device(sys.path(), "../../devices/platform/usb/block/sda/sda2", true);
        // The live incident: seven stacked mounts of the same devnode.
        let mounts: Vec<(String, String)> = (0..7)
            .map(|_| {
                (
                    dev.display().to_string(),
                    "/run/media/user/uuid".to_owned(),
                )
            })
            .collect();
        let (steps, power_off) = build_shutdown_steps(&dev, &sys_block_dir, &mounts);
        // Sync first, then ONE UNMOUNT PER STACKED LAYER (a single
        // unmount removes a single layer — the live failure had seven).
        assert_eq!(steps.len(), 1 + 7, "sync + one unmount per layer");
        assert_eq!(steps[0].program, "sync");
        assert!(steps[1..].iter().all(|step| step.program == "udisksctl"
            && step.args
                == vec![
                    "unmount".to_owned(),
                    "-b".to_owned(),
                    dev.display().to_string()
                ]));
        // Power-off targets the whole disk, not the partition, and comes
        // back SEPARATELY: the caller runs it under sudo after the drain,
        // so it is strictly the last step.
        assert_eq!(power_off.program, "udisksctl");
        assert_eq!(
            power_off.args,
            vec!["power-off".to_owned(), "-b".to_owned(), "/dev/sda".to_owned()]
        );
    }

    #[test]
    fn shutdown_drain_is_bounded_and_covers_an_empty_mount_table() {
        let sys = TempDir::new().unwrap();
        let (dev, sys_block_dir) =
            fake_block_device(sys.path(), "../../devices/platform/usb/block/sda/sda2", true);
        let mounts: Vec<(String, String)> = (0..EJECT_DRAIN_LIMIT * 2)
            .map(|_| {
                (
                    dev.display().to_string(),
                    "/run/media/user/uuid".to_owned(),
                )
            })
            .collect();
        let (steps, _) = build_shutdown_steps(&dev, &sys_block_dir, &mounts);
        assert_eq!(steps.len(), 1 + EJECT_DRAIN_LIMIT, "drain cap holds");
        // An unreadable/empty snapshot still yields the sync bookend and
        // the final power-off (which then fails honestly if busy).
        let (empty, power_off) = build_shutdown_steps(&dev, &sys_block_dir, &[]);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].program, "sync");
        assert_eq!(power_off.program, "udisksctl");
    }

    #[test]
    fn shutdown_step_errors_are_named_and_uniform() {
        // Round 6.2: every shutdown step fails with the same
        // «шаг <name> (sudo): …» shape; udisksctl subcommands are told
        // apart by argv.
        let sync = CommandSpec::new("sync", &[]);
        let unmount = CommandSpec::new("udisksctl", &["unmount", "-b", "/dev/sda2"]);
        let power_off = CommandSpec::new("udisksctl", &["power-off", "-b", "/dev/sda"]);
        assert_eq!(shutdown_step_label(&sync), "sync");
        assert_eq!(shutdown_step_label(&unmount), "unmount");
        assert_eq!(shutdown_step_label(&power_off), "power-off");
        assert_eq!(
            shutdown_step_error(&unmount, "  Error: The drive is in use  "),
            "шаг unmount (sudo): Error: The drive is in use"
        );
        // A spawn failure reuses the same uniform shape.
        assert_eq!(
            shutdown_step_error(&power_off, "Нет такого файла или процесса (os error 2)"),
            "шаг power-off (sudo): Нет такого файла или процесса (os error 2)"
        );
    }

    #[test]
    fn wrong_password_markers_are_classified() {
        assert_eq!(
            classify_mount_failure("sudo: 1 incorrect password attempt"),
            MountFailure::WrongPassword
        );
        assert_eq!(
            classify_mount_failure("Sorry, try again."),
            MountFailure::WrongPassword
        );
        assert_eq!(
            classify_mount_failure("sudo: no password was provided"),
            MountFailure::WrongPassword
        );
    }

    #[test]
    fn dirty_journal_markers_are_classified() {
        assert_eq!(
            classify_mount_failure("hfsplus: journal is not empty"),
            MountFailure::DirtyJournal
        );
        assert_eq!(
            classify_mount_failure("hfsplus: uninitialized node"),
            MountFailure::DirtyJournal
        );
    }

    #[test]
    fn busy_and_mounted_markers_are_classified() {
        assert_eq!(
            classify_mount_failure("mount: /run/media/u/x: already mounted on /run/media/u/x."),
            MountFailure::AlreadyMounted
        );
        assert_eq!(
            classify_mount_failure("mount: /run/media/u/x is busy"),
            MountFailure::AlreadyMounted
        );
    }

    #[test]
    fn unknown_stderr_is_other() {
        assert_eq!(
            classify_mount_failure("mount: unknown filesystem type 'hfsx'"),
            MountFailure::Other
        );
        assert_eq!(classify_mount_failure(""), MountFailure::Other);
    }

    #[test]
    fn mount_output_success_is_ok() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(classify_mount_output(&output).is_ok());
    }

    #[test]
    fn mount_output_failure_maps_to_kind() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: Vec::new(),
            stderr: b"hfsplus: journal is not empty\n".to_vec(),
        };
        assert!(matches!(
            classify_mount_output(&output),
            Err(MountError::DirtyJournal)
        ));
    }

    #[test]
    fn hints_exist_for_recoverable_errors() {
        assert!(MountError::DirtyJournal.hint().unwrap().contains("fsck.hfsplus"));
        assert!(MountError::WrongPassword.hint().is_some());
        assert!(MountError::AlreadyMounted.hint().is_some());
        assert!(MountError::Failed.hint().is_none());
    }

    #[test]
    fn current_uid_gid_is_available_on_linux() {
        let (uid, _gid) = current_uid_gid().expect("/proc/self exists on Linux");
        assert_ne!(uid, u32::MAX);
    }
}
