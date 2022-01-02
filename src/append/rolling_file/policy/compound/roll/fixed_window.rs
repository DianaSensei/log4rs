//! The fixed-window roller.
//!
//! Requires the `fixed_window_roller` feature.

use anyhow::bail;
#[cfg(feature = "background_rotation")]
use parking_lot::{Condvar, Mutex};
#[cfg(feature = "background_rotation")]
use std::sync::Arc;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::append::rolling_file::policy::compound::roll::Roll;
#[cfg(feature = "config_parsing")]
use crate::config::{Deserialize, Deserializers};

/// Configuration for the fixed window roller.
#[cfg(feature = "config_parsing")]
#[derive(Clone, Eq, PartialEq, Hash, Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixedWindowRollerConfig {
    pattern: String,
    base: Option<u32>,
    count: u32,
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
enum Compression {
    None,
    #[cfg(feature = "gzip")]
    Gzip,
}

impl Compression {
    fn compress(&self, src: &Path, dst: &str) -> io::Result<()> {
        match *self {
            Compression::None => move_file(src, dst),
            #[cfg(feature = "gzip")]
            Compression::Gzip => {
                #[cfg(feature = "flate2")]
                use flate2::write::GzEncoder;
                use std::fs::File;

                let mut i = File::open(src)?;

                let o = File::create(dst)?;
                let mut o = GzEncoder::new(o, flate2::Compression::default());

                io::copy(&mut i, &mut o)?;
                drop(o.finish()?);
                drop(i); // needs to happen before remove_file call on Windows

                fs::remove_file(src)
            }
        }
    }
}

/// A roller which maintains a fixed window of archived log files.
///
/// A `FixedWindowRoller` is configured with a filename pattern, a base index,
/// and a maximum file count. Each archived log file is associated with a numeric
/// index ordering it by age, starting at the base index. Archived log files are
/// named by substituting all instances of `{}` with the file's index in the
/// filename pattern.
///
/// For example, if the filename pattern is `archive/foo.{}.log`, the base index
/// is 0 and the count is 2, the first log file will be archived as
/// `archive/foo.0.log`. When the next log file is archived, `archive/foo.0.log`
/// will be renamed to `archive/foo.1.log` and the new log file will be named
/// `archive/foo.0.log`. When the third log file is archived,
/// `archive/foo.1.log` will be deleted, `archive/foo.0.log` will be renamed to
/// `archive/foo.1.log`, and the new log file will be renamed to
/// `archive/foo.0.log`.
///
/// If the file extension of the pattern is `.gz` and the `gzip` Cargo feature
/// is enabled, the archive files will be gzip-compressed.
///
/// Note that this roller will have to rename every archived file every time the
/// log rolls over. Performance may be negatively impacted by specifying a large
/// count.
#[derive(Clone, Debug)]
pub struct FixedWindowRoller {
    pattern: String,
    compression: Compression,
    base: u32,
    count: u32,
    #[cfg(feature = "background_rotation")]
    cond_pair: Arc<(Mutex<bool>, Condvar)>,
}

impl FixedWindowRoller {
    /// Returns a new builder for the `FixedWindowRoller`.
    pub fn builder() -> FixedWindowRollerBuilder {
        FixedWindowRollerBuilder { base: 0 }
    }
}

impl Roll for FixedWindowRoller {
    #[cfg(not(feature = "background_rotation"))]
    fn roll(&self, file: &Path, roll_type: i16) -> anyhow::Result<()> {
        if self.count == 0 {
            return fs::remove_file(file).map_err(Into::into);
        }

        rotate(
            self.pattern.clone(),
            self.compression,
            self.base,
            roll_type,
            file.to_path_buf(),
        )?;

        Ok(())
    }

    #[cfg(feature = "background_rotation")]
    fn roll(&self, file: &Path, roll_type: i16) -> anyhow::Result<()> {
        if self.count == 0 {
            return fs::remove_file(file).map_err(Into::into);
        }

        // rename the file
        let temp = make_temp_file_name(file);
        move_file(file, &temp)?;

        // Wait for the state to be ready to roll
        let &(ref lock, ref cvar) = &*self.cond_pair.clone();
        let mut ready = lock.lock();
        if !*ready {
            cvar.wait(&mut ready);
        }
        *ready = false;
        drop(ready);

        let pattern = self.pattern.clone();
        let compression = self.compression;
        let base = self.base;
        let count = self.count;
        let cond_pair = self.cond_pair.clone();
        // rotate in the separate thread
        std::thread::spawn(move || {
            let &(ref lock, ref cvar) = &*cond_pair;
            let mut ready = lock.lock();

            if let Err(e) = rotate(pattern, compression, base, roll_type, temp) {
                use std::io::Write;
                let _ = writeln!(io::stderr(), "log4rs, error rotating: {}", e);
            }
            *ready = true;
            cvar.notify_one();
        });

        Ok(())
    }
}

fn move_file<P, Q>(src: P, dst: Q) -> io::Result<()>
where
    P: AsRef<Path>,
    Q: AsRef<Path>,
{
    // first try a rename
    match fs::rename(src.as_ref(), dst.as_ref()) {
        Ok(()) => return Ok(()),
        Err(ref e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }

    // fall back to a copy and delete if src and dst are on different mounts
    fs::copy(src.as_ref(), dst.as_ref()).and_then(|_| fs::remove_file(src.as_ref()))
}

#[cfg(feature = "background_rotation")]
fn make_temp_file_name<P>(file: P) -> PathBuf
where
    P: AsRef<Path>,
{
    let mut n = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0))
        .as_secs();
    let mut temp = file.as_ref().to_path_buf();
    temp.set_extension(format!("{}", n));
    while temp.exists() {
        n += 1;
        temp.set_extension(format!("{}", n));
    }
    temp
}

// TODO(eas): compress to tmp file then move into place once prev task is done
fn rotate(
    pattern: String,
    compression: Compression,
    mut base: u32,
    rotate_type: i16,
    file: PathBuf,
) -> io::Result<()> {
    let day = if rotate_type == 1 {
        chrono::Utc::today() + chrono::Duration::days(-1)
    } else {
        chrono::Utc::today()
    };

    let year_month = day.format("%Y-%m").to_string();
    let year_month_day = day.format("%Y-%m-%d").to_string();

    let dst_0 = pattern.replace("{yyyy-mm}", &year_month);
    let dst_0 = dst_0.replace("{yyyy-mm-dd}", &year_month_day);

    let mut dst = dst_0.replace("{}", &base.to_string());

    if let Some(parent) = Path::new(&dst_0).parent() {
        fs::create_dir_all(parent)?;
    }

    while Path::new(&dst).exists() {
        base += 1;
        dst = dst_0.replace("{}", &base.to_string());
    }

    compression.compress(&file, &dst).map_err(|e| {
        println!("err compressing: {:?}, dst: {:?}", file, dst);
        e
    })?;
    Ok(())
}

/// A builder for the `FixedWindowRoller`.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default)]
pub struct FixedWindowRollerBuilder {
    base: u32,
}

impl FixedWindowRollerBuilder {
    /// Sets the base index for archived log files.
    ///
    /// Defaults to 0.
    pub fn base(mut self, base: u32) -> FixedWindowRollerBuilder {
        self.base = base;
        self
    }

    /// Constructs a new `FixedWindowRoller`.
    ///
    /// `pattern` is either an absolute path or lacking a leading `/`, relative
    /// to the `cwd` of your application. The pattern must contain at least one
    /// instance of `{}`, all of which will be replaced with an archived log file's index.
    ///
    /// If the file extension of the pattern is `.gz` and the `gzip` Cargo
    /// feature is enabled, the archive files will be gzip-compressed.
    /// If the extension is `.gz` and the `gzip` feature is *not* enabled, an error will be returned.
    pub fn build(self, pattern: &str, count: u32) -> anyhow::Result<FixedWindowRoller> {
        if !pattern.contains("{}") {
            bail!("pattern does not contain `{}`");
        }

        let compression = match Path::new(pattern).extension() {
            #[cfg(feature = "gzip")]
            Some(e) if e == "gz" => Compression::Gzip,
            #[cfg(not(feature = "gzip"))]
            Some(e) if e == "gz" => {
                bail!("gzip compression requires the `gzip` feature");
            }
            _ => Compression::None,
        };

        Ok(FixedWindowRoller {
            pattern: pattern.to_owned(),
            compression,
            base: self.base,
            count,
            #[cfg(feature = "background_rotation")]
            cond_pair: Arc::new((Mutex::new(true), Condvar::new())),
        })
    }
}

/// A deserializer for the `FixedWindowRoller`.
///
/// # Configuration
///
/// ```yaml
/// kind: fixed_window
///
/// # The filename pattern for archived logs. This is either an absolute path or if lacking a leading `/`,
/// # relative to the `cwd` of your application. The pattern must contain at least one
/// # instance of `{}`, all of which will be replaced with an archived log file's index.
/// # If the file extension of the pattern is `.gz` and the `gzip` Cargo feature
/// # is enabled, the archive files will be gzip-compressed.
/// # Required.
/// pattern: archive/foo.{}.log
///
/// # The maximum number of archived logs to maintain. Required.
/// count: 5
///
/// # The base value for archived log indices. Defaults to 0.
/// base: 1
/// ```
#[cfg(feature = "config_parsing")]
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default)]
pub struct FixedWindowRollerDeserializer;

#[cfg(feature = "config_parsing")]
impl Deserialize for FixedWindowRollerDeserializer {
    type Trait = dyn Roll;

    type Config = FixedWindowRollerConfig;

    fn deserialize(
        &self,
        config: FixedWindowRollerConfig,
        _: &Deserializers,
    ) -> anyhow::Result<Box<dyn Roll>> {
        let mut builder = FixedWindowRoller::builder();
        if let Some(base) = config.base {
            builder = builder.base(base);
        }

        Ok(Box::new(builder.build(&config.pattern, config.count)?))
    }
}

#[cfg(test)]
mod test {
    use std::{
        fs::File,
        io::{Read, Write},
    };

    use super::*;
    use crate::append::rolling_file::policy::compound::roll::Roll;

    #[cfg(feature = "background_rotation")]
    fn wait_for_roller(roller: &FixedWindowRoller) {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _lock = roller.cond_pair.0.lock();
    }

    #[cfg(not(feature = "background_rotation"))]
    fn wait_for_roller(_roller: &FixedWindowRoller) {}

    #[test]
    #[cfg_attr(feature = "gzip", ignore)]
    fn unsupported_gzip() {
        let dir = tempfile::tempdir().unwrap();

        let pattern = dir.path().join("{}.gz");
        assert!(FixedWindowRoller::builder()
            .build(pattern.to_str().unwrap(), 2)
            .is_err());
    }

    #[test]
    #[cfg_attr(not(feature = "gzip"), ignore)]
    // or should we force windows user to install gunzip
    #[cfg(not(windows))]
    fn supported_gzip() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();

        let pattern = dir.path().join("{}.gz");
        let roller = FixedWindowRoller::builder()
            .build(pattern.to_str().unwrap(), 2)
            .unwrap();

        let contents = (0..10000).map(|i| i as u8).collect::<Vec<_>>();

        let file = dir.path().join("foo.log");
        File::create(&file).unwrap().write_all(&contents).unwrap();

        roller.roll(&file, 1).unwrap();
        wait_for_roller(&roller);

        assert!(Command::new("gunzip")
            .arg(dir.path().join("0.gz"))
            .status()
            .unwrap()
            .success());

        let mut file = File::open(dir.path().join("0")).unwrap();
        let mut actual = vec![];
        file.read_to_end(&mut actual).unwrap();

        assert_eq!(contents, actual);
    }
}
