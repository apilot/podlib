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
/// constructors ([`build_mount_command`], [`build_unmount_sequence`]) so
/// tests can assert exact command lines without executing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command
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
    let mut mounted_devs: Vec<Option<&Path>> = Vec::new();
    for (candidate, dev) in &mounted {
        mounted_devs.push(dev.as_deref());
        out.push((candidate.clone(), dev.clone()));
    }
    for disk in disks {
        if mounted_devs
            .iter()
            .any(|dev| *dev == Some(disk.partition.as_path()))
        {
            continue;
        }
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

/// Safe-eject sequence from the runbook: flush buffers, unmount the
/// volume, power off the whole USB disk (so the device firmware shows
/// its eject screen). `udisksctl` needs no sudo for removable media owned
/// by the active user. `_mount` is reserved for symmetry with the mount
/// flow (udisksctl unmounts by device here). `sys_block_dir` is the
/// injectable sysfs root used to resolve the whole-disk device — see
/// [`parent_disk`].
pub fn build_unmount_sequence(
    _mount: &Path,
    block_dev: &Path,
    sys_block_dir: &Path,
) -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("sync", &[]),
        CommandSpec::new("udisksctl", &["unmount", "-b", &block_dev.display().to_string()]),
        CommandSpec::new(
            "udisksctl",
            &[
                "power-off",
                "-b",
                &parent_disk(block_dev, sys_block_dir).display().to_string(),
            ],
        ),
    ]
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

/// Runs the mount under sudo, feeding the password over the stdin pipe.
/// The password lives only in the child's memory for the duration of the
/// command; it is never logged, persisted, or placed in argv.
pub fn run_sudo_mount(password: &str, spec: &CommandSpec) -> std::io::Result<Output> {
    let mut child = build_sudo_command(spec).spawn()?;
    {
        // `build_sudo_command` always pipes stdin; `None` here would mean
        // an internal bug, so fail with an error instead of panicking.
        let stdin = child.stdin.as_mut().ok_or_else(|| {
            std::io::Error::other("stdin не открыт: build_sudo_command не настроил pipe")
        })?;
        stdin.write_all(password.as_bytes())?;
        stdin.write_all(b"\n")?;
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

/// A failed eject step with the tool's own output for display.
#[derive(Debug, thiserror::Error)]
#[error("шаг {step_index} ({program}) завершился с ошибкой: {message}")]
pub struct SequenceError {
    pub step_index: usize,
    pub program: String,
    pub message: String,
}

/// Runs steps in order, failing fast. Blocking — execute on a worker
/// thread, never on the UI thread.
pub fn run_sequence(steps: &[CommandSpec]) -> Result<(), SequenceError> {
    for (step_index, step) in steps.iter().enumerate() {
        let output = step.to_command().output().map_err(|error| SequenceError {
            step_index,
            program: step.program.clone(),
            message: error.to_string(),
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let message = if stderr.trim().is_empty() {
                stdout.trim().to_owned()
            } else {
                stderr.trim().to_owned()
            };
            return Err(SequenceError {
                step_index,
                program: step.program.clone(),
                message,
            });
        }
    }
    Ok(())
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
    fn unmount_sequence_is_sync_unmount_power_off() {
        let sys = TempDir::new().unwrap();
        let (dev, sys_block_dir) =
            fake_block_device(sys.path(), "../../devices/platform/usb/block/sda/sda2", true);
        let steps = build_unmount_sequence(Path::new("/run/media/user/uuid"), &dev, &sys_block_dir);
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].program, "sync");
        assert_eq!(
            steps[1].args,
            vec!["unmount".to_owned(), "-b".to_owned(), dev.display().to_string()]
        );
        // power-off targets the whole disk, not the partition.
        assert_eq!(
            steps[2].args,
            vec!["power-off".to_owned(), "-b".to_owned(), "/dev/sda".to_owned()]
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
