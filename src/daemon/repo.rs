use std::{io, fs};
use std::fs::File;
use std::num::ParseIntError;
use std::ops::Deref;
use std::path::PathBuf;
use std::str::FromStr;
use rpki::uri;
use crate::remote::publication::pubmsg::Message;
use crate::remote::publication::query::{PublishElement, PublishQuery};
use crate::remote::publication::reply::{ListElement, ListReply, SuccessReply};
use crate::util::file::{self, CurrentFile, RecursorError};
use crate::util::xml::{AttributesError, XmlReader, XmlReaderErr, XmlWriter};

const VERSION: &'static str = "1";
const NS: &'static str = "http://www.ripe.net/rpki/rrdp";
const FS_FOLDER: &'static str = "rsync";
pub const RRDP_FOLDER: &'static str = "rrdp";

/// Derives the notification uri based on the RRDP base uri (from config)
/// Panics in case of (config) issues, and is called during bootstrapping.
pub fn notification_uri(base: &uri::Http) -> uri::Http {
    let base_string = base.to_string();
    if ! base_string.ends_with("/") {
        panic!("RRDP base path should end with a '/', got:{}", base_string);
    }
    uri::Http::from_string(
        format!("{}notification.xml", base.to_string())
    ).unwrap() // Can only fail at startup if mis-configured.
}


//------------ Repository ----------------------------------------------------

/// This type orchestrates publishing in both an RSYNC and RRDP
/// (RFC8182) format.
#[derive(Clone, Debug)]
pub struct Repository {
    // file_store
    fs: FileStore,

    // RRDP
    rrdp: RrdpServer
}

/// # Construct
///
impl Repository {
    pub fn new(
        rrdp_base_uri: &uri::Http,
        work_dir: &PathBuf
    ) -> Result<Self, Error>
    {
        let fs = FileStore::new(work_dir)?;
        let rrdp = RrdpServer::new(rrdp_base_uri, work_dir)?;
        Ok( Repository { fs, rrdp } )
    }
}

/// # Publish / List
///
impl Repository {
    /// Publishes an publish query and returns a success reply embedded in
    /// a message. Throws an error in case of issues. The PubServer needs
    /// to wrap such errors in a response message to the publisher.
    pub fn publish(
        &mut self,
        update: &PublishQuery,
        base_uri: &uri::Rsync
    ) -> Result<Message, Error> {
        debug!("Processing update with {} elements", update.elements().len());
        self.fs.publish(update, base_uri)?;
        self.rrdp.publish(update)?;
        Ok(SuccessReply::build_message())
    }

    /// Lists the objects for a base_uri, presumably all for the same
    /// publisher.
    pub fn list(
        &self,
        base_uri: &uri::Rsync
    ) -> Result<Message, Error> {
        debug!("Processing list query");
        let files = self.fs.list(base_uri)?;
        let mut builder = ListReply::build();
        for file in files {
            builder.add(
                ListElement::reply(
                    file.content(),
                    file.uri().clone()
                )
            )
        }
        debug!("Found {} files", builder.len());
        Ok(builder.build_message())
    }
}


//------------ FileStore -----------------------------------------------------

/// This type is responsible for publishing files on disk in a structure so
/// that an rscynd can be set up to serve this (RPKI) data. Note that the
/// rsync host name and module are part of the path, so make sure that the
/// rsyncd modules and paths are setup properly for each supported rsync
/// base uri used.
#[derive(Clone, Debug)]
pub struct FileStore {
    base_dir: PathBuf
}

/// # Construct
///
impl FileStore {
    pub fn new(work_dir: &PathBuf) -> Result<Self, Error> {
        let mut rsync_dir = PathBuf::from(work_dir);
        rsync_dir.push(FS_FOLDER);
        if ! rsync_dir.is_dir() {
            fs::create_dir_all(&rsync_dir)?;
        }
        Ok ( FileStore { base_dir: rsync_dir } )
    }
}

/// # Publishing
///
impl FileStore {
    /// Process a PublishQuery update
    pub fn publish(
        &mut self,
        update: &PublishQuery,
        base_uri: &uri::Rsync
    ) -> Result<(), Error> {
        self.verify_query(update, base_uri)?;
        self.update_files(update)?;

        Ok(())
    }

    pub fn list(
        &self,
        base_uri: &uri::Rsync
    ) -> Result<Vec<CurrentFile>, Error> {
        let path = self.path_for_publisher(base_uri);

        if !path.exists() {
            Ok(Vec::new())
        } else {
            file::crawl_incl_rsync_base(&path, base_uri)
                .map_err(|e| Error::RecursorError(e))
        }
    }

    /// Assert that all updates are confined to the given base_uri; i.e. do
    /// not allow publishers to update things outside of their own jail.
    fn verify_query(
        &self,
        update: &PublishQuery,
        base_uri: &uri::Rsync
    ) -> Result<(), Error> {
        for q in update.elements() {
            match q {
                PublishElement::Publish(p) => {
                    Self::assert_uri(base_uri, p.uri())?;
                    if self.get_current_file_opt(p.uri()).is_some() {
                        return Err(Error::ObjectAlreadyPresent(p.uri().clone()))
                    }
                },
                PublishElement::Update(u) => {
                    Self::assert_uri(base_uri, u.uri())?;
                    if let Some(cur) = self.get_current_file_opt(u.uri()) {
                        if cur.hash() != u.hash() {
                            return Err(Error::NoObjectMatchingHash)
                        }
                    } else {
                        return Err(Error::NoObjectPresent(u.uri().clone()))
                    }
                },
                PublishElement::Withdraw(w) => {
                    Self::assert_uri(base_uri, w.uri())?;
                    if let Some(cur) = self.get_current_file_opt(w.uri()) {
                        if cur.hash() != w.hash() {
                            return Err(Error::NoObjectMatchingHash)
                        }
                    } else {
                        return Err(Error::NoObjectPresent(w.uri().clone()))
                    }
                },
            }
        }

        debug!("Update is consistent with current state");
        Ok(())
    }

    /// Perform the actual updates on disk. This assumes that the updates
    /// have been verified.
    fn update_files(
        &self,
        update: &PublishQuery
    ) -> Result<(), Error> {
        for q in update.elements() {
            match q {
                PublishElement::Publish(p) => {
                    debug!("Saving file for uri: {}", p.uri().to_string());
                    file::save_with_rsync_uri(
                        p.object(),
                        &self.base_dir,
                        p.uri()
                    )?;
                },
                PublishElement::Update(u) => {
                    debug!("Updating file for uri: {}", u.uri().to_string());
                    file::save_with_rsync_uri(
                        u.object(),
                        &self.base_dir,
                        u.uri()
                    )?;
                },
                PublishElement::Withdraw(w) => {
                    debug!("Withdrawing file for uri: {}", w.uri().to_string());
                    file::delete_with_rsync_uri(
                        &self.base_dir,
                        w.uri()
                    )?;
                },
            }
        }
        Ok(())
    }

    fn assert_uri(base: &uri::Rsync, file: &uri::Rsync) -> Result<(), Error> {
        if base.module() == file.module() &&
            file.path().starts_with(base.path()) {
            Ok(())
        } else {
            Err(Error::OutsideBaseUri)
        }
    }

    fn get_current_file_opt(
        &self,
        file_uri: &uri::Rsync
    ) -> Option<CurrentFile> {
        match file::read_with_rsync_uri(&self.base_dir, file_uri) {
            Ok(bytes) => Some(CurrentFile::new(file_uri.clone(), bytes)),
            Err(_) => None
        }
    }

    // Returns the relative sub-dir that we should scan for this particular
    // publisher.
    fn path_for_publisher(&self, file_uri: &uri::Rsync) -> PathBuf {
        let mut path = self.base_dir.clone();
        let module = file_uri.module();
        path.push(PathBuf::from(module.authority()));
        path.push(PathBuf::from(module.module()));
        path.push(PathBuf::from(file_uri.path()));
        path
    }
}


/// This type publishes RRDP notifications, snapshots and deltas so that they
/// can be served to relying parties.
#[derive(Clone, Debug)]
pub struct RrdpServer {
    // The base URI path for notification, snapshot and delta files.
    base_uri:  uri::Http,

    // Dir for notification, snapshot and delta files.
    rrdp_base: PathBuf,

    // Dir for file_store (so that snapshots can be derived)
    fs_base:   PathBuf
}

/// # Setup and initialisation
impl RrdpServer {

    /// Creates a new RrdpServer.
    ///
    /// This will pick up the saved state from the notification.xml if
    /// present, or initialise a new server with a random session_id,
    /// starting at serial 1, and including a snapshot for everything
    /// currently stored in the rsync file_store.
    pub fn new(
        base_uri: &uri::Http,
        work_dir: &PathBuf
    ) -> Result<Self, Error>
    {
        let rrdp_base = file::sub_dir(work_dir, RRDP_FOLDER)?;
        let fs_base = file::sub_dir(work_dir, FS_FOLDER)?;
        Ok(RrdpServer { base_uri: base_uri.clone(), rrdp_base, fs_base })
    }
}

/// # Publishing
///
impl RrdpServer {

    /// Process an update PublishQuery and produce a new delta, snapshot
    /// and notification file. Assumes that this is called *after* the
    /// ['FileStore'] has published, so files should already be saved to
    /// disk and the snapshots can be derived from this.
    pub fn publish(&mut self, update: &PublishQuery) -> Result<(), Error> {
        let current_notification = Notification::derive(
            &self.notification_path(),
            &self.base_uri,
            &self.rrdp_base
        );

        let session_id = match &current_notification {
            Some(n) => n.session_id.clone(),
            None => {
                use rand::{thread_rng, Rng};
                let mut rng = thread_rng();
                let rnd: u32 = rng.gen();
                format!("{}", rnd)
            }
        };
        let serial = match &current_notification {
            Some(n) => n.serial + 1,
            None    => 1
        };

        let snapshot = self.save_snapshot(&session_id, serial)?;
        let delta = self.save_delta(&session_id, serial, update)?;

        let mut notif_builder = NotificationBuilder::new();

        notif_builder.with_session_id(session_id);
        notif_builder.with_serial(serial);
        notif_builder.with_snapshot(snapshot);

        if let Some(notification) = current_notification {
            notif_builder.with_deltas(notification.deltas)
        }

        notif_builder.add_delta_to_start(delta);

        let notification = notif_builder.build();
        notification.save(&self.notification_path())
    }


    /// Saves the RFC8181 PublishQuery as an RFC8182 delta file.
    fn save_delta(
        &mut self,
        session_id: &String,
        serial: usize,
        update: &PublishQuery
    ) -> Result<DeltaRef, Error>
    {
        let path = self.delta_path(session_id, serial);
        debug!("Writing delta: {}", path.to_string_lossy());
        let mut file = file::create_file_with_path(&path)?;

        XmlWriter::encode_to_file(& mut file, |w| {

            let a = [
                ("xmlns", NS),
                ("version", VERSION),
                ("session_id", session_id),
                ("serial", &format!("{}", serial)),
            ];

            w.put_element(
                "delta",
                Some(&a),
                |w| {
                    for el in update.elements() {
                        match el {
                            PublishElement::Publish(el) => {
                                let uri = el.uri().to_string();
                                let a = [
                                    ("uri", uri.as_ref())
                                ];
                                w.put_element(
                                    "publish",
                                    Some(&a),
                                    |w| {
                                        w.put_blob(el.object())
                                    }
                                )?
                            },
                            PublishElement::Update(el) => {
                                let uri = el.uri().to_string();
                                let hash = hex::encode(el.hash());
                                let a = [
                                    ("uri", uri.as_ref()),
                                    ("hash", hash.as_ref())
                                ];
                                w.put_element(
                                    "publish",
                                    Some(&a),
                                    |w| {
                                        w.put_blob(el.object())
                                    }
                                )?
                            },
                            PublishElement::Withdraw(el) => {
                                let uri = el.uri().to_string();
                                let hash = hex::encode(el.hash());
                                let a = [
                                    ("uri", uri.as_ref()),
                                    ("hash", hash.as_ref())
                                ];
                                w.put_element(
                                    "withdraw",
                                    Some(&a),
                                    |w| {
                                        w.empty()
                                    }
                                )?
                            },
                        };
                    }
                    Ok(())
                })
        })?;

        let file_info = FileInfo::for_path(
            &path,
            &self.base_uri,
            &self.rrdp_base
        )?;
        Ok(DeltaRef { serial, file_info })
    }


    /// Saves the current snapshot, based on the state of the ['FileStore']
    /// base directory.
    fn save_snapshot(
        &mut self,
        session_id: &String,
        serial: usize
    ) -> Result<SnapshotRef, Error> {
        let path = self.snapshot_path(session_id, serial);
        debug!("Writing snapshot: {}", path.to_string_lossy());
        let mut file = file::create_file_with_path(&path)?;
        let current_files = file::crawl_derive_rsync_uri(&self.fs_base)?;

        XmlWriter::encode_to_file(& mut file, |w| {

            let a = [
                ("xmlns", NS),
                ("version", VERSION),
                ("session_id", session_id),
                ("serial", &format!("{}", serial)),
            ];

            w.put_element(
                "snapshot",
                Some(&a),
                |w| {
                    for cf in current_files {
                        let uri = cf.uri().to_string();
                        let a = [ ("xmlns", uri.as_ref()) ];
                        w.put_element(
                            "publish",
                            Some(&a),
                            |w| {
                                w.put_blob(cf.content())
                            }
                        )?;
                    }
                    Ok(())
                }
            )
        })?;

        let file_info = FileInfo::for_path(
            &path,
            &self.base_uri,
            &self.rrdp_base
        )?;

        Ok(SnapshotRef { file_info })
    }


    pub fn notification_path(&self) -> PathBuf {
        let mut path = self.rrdp_base.clone();
        path.push("notification.xml");
        path
    }

    pub fn delta_path(&self, session: &str, serial: usize) -> PathBuf {
        let mut path = self.serial_path(session, serial);
        path.push("delta.xml");
        path
    }

    pub fn snapshot_path(&self, session: &str, serial: usize) -> PathBuf {
        let mut path = self.serial_path(session, serial);
        path.push("snapshot.xml");
        path
    }

    fn serial_path(&self, session: &str, serial: usize) -> PathBuf {
        let mut path = self.rrdp_base.clone();
        path.push(session);
        path.push(format!("{}", serial));
        path
    }
}


//------------ Notification --------------------------------------------------

#[derive(Clone, Debug)]
pub struct Notification {
    session_id: String,
    serial:     usize,
    snapshot:   SnapshotRef,
    deltas:     Vec<DeltaRef>
}

/// # Accessors
///
impl Notification {
    pub fn serial(&self) -> &usize {
        &self.serial
    }

    pub fn deltas(&self) -> &Vec<DeltaRef> {
        &self.deltas
    }
}

/// # Load and save
///
impl Notification {

    pub fn derive(
        path: &PathBuf,
        base_uri: &uri::Http,
        rrdp_base: &PathBuf
    ) -> Option<Notification> {
        let mut builder = NotificationBuilder::new();

        match XmlReader::open(path, |r| -> Result<(), Error> {
            r.take_named_element("notification", |mut a, r| {
                {
                    // process attributes
                    builder.with_session_id(a.take_req("session_id")?);
                    let serial = usize::from_str(a.take_req("serial")?.as_ref())?;
                    builder.with_serial(serial);
                    // about NS
                }

                {
                    // expect snapshot ref
                    r.take_named_element(
                        "snapshot",
                        |mut a, _r| -> Result<(), Error> {
                            let uri = uri::Http::from_string(a.take_req("uri")?)?;
                            let file_info = FileInfo::for_uri(
                                &uri,
                                base_uri,
                                rrdp_base
                            )?;

                            builder.with_snapshot(
                                SnapshotRef { file_info }
                            );
                            Ok(())
                        })?;
                }

                {
                    // deltas
                    loop {
                        let d = r.take_opt_element(|t, mut a, _r| {
                            match t.name.as_ref() {
                                "delta" => {
                                    let uri = uri::Http::from_string(
                                        a.take_req("uri")?
                                    )?;
                                    let serial = usize::from_str(
                                        a.take_req("serial")?.as_ref()
                                    )?;
                                    let file_info = FileInfo::for_uri(
                                        &uri,
                                        base_uri,
                                        rrdp_base
                                    )?;

                                    Ok(Some(DeltaRef {
                                        serial,
                                        file_info
                                    }))
                                },
                                _ => Err(Error::NotificationFileError)
                            }
                        })?;
                        match d {
                            None => break,
                            Some(d) => builder.add_delta(d)
                        }
                    }
                    Ok(())
                }
            })
        }).map_err(|_| Error::NotificationFileError) {
            Ok(_) => Some(builder.build()),
            Err(_) => None
        }
    }

    /// Saves a notification file as RFC8182 XML.
    fn save(&self, path: &PathBuf) -> Result<(), Error> {
        debug!("Writing notification file: {}", path.to_string_lossy());
        let mut file = file::create_file_with_path(&path)?;

        XmlWriter::encode_to_file(& mut file, |w| {

            let a = [
                ("xmlns", NS),
                ("version", VERSION),
                ("session_id", self.session_id.as_ref()),
                ("serial", &format!("{}", self.serial)),
            ];

            w.put_element(
                "notification",
                Some(&a),
                |w| {
                    {
                        // snapshot ref
                        let uri = self.snapshot.uri.to_string();
                        let hash = &self.snapshot.hash;
                        let a = [
                            ("uri", uri.as_str()),
                            ("hash", hash)
                        ];
                        w.put_element(
                            "snapshot",
                            Some(&a),
                            |w| { w.empty() }
                        )?;
                    }

                    {
                        // delta refs
                        for delta in &self.deltas {
                            let serial = format!("{}", delta.serial);
                            let uri = delta.uri.to_string();
                            let hash = &delta.hash;
                            let a = [
                                ("serial", serial.as_ref()),
                                ("uri", uri.as_str()),
                                ("hash", hash)
                            ];
                            w.put_element(
                                "delta",
                                Some(&a),
                                |w| { w.empty() }
                            )?;
                        }
                    }

                    Ok(())
                }
            )
        })?;

        Ok(())
    }
}


//------------ SnapshotRef ---------------------------------------------------

#[derive(Clone, Debug)]
pub struct SnapshotRef {
    file_info:  FileInfo
}

impl Deref for SnapshotRef {
    type Target = FileInfo;

    fn deref(&self) -> &FileInfo {
        &self.file_info
    }
}


//------------ DeltaRef ------------------------------------------------------

#[derive(Clone, Debug)]
pub struct DeltaRef {
    serial:     usize,
    file_info:  FileInfo,
}

impl DeltaRef {
    pub fn serial(&self) -> &usize {
        &self.serial
    }
}


impl Deref for DeltaRef {
    type Target = FileInfo;

    fn deref(&self) -> &FileInfo {
        &self.file_info
    }
}


//------------ FileInfo ------------------------------------------------------

#[derive(Clone, Debug)]
pub struct FileInfo {
    uri:   uri::Http,
    hash:  String,
    size:  usize
}

impl FileInfo {
    fn new(uri: uri::Http, hash: String, size: usize) -> FileInfo {
        FileInfo { uri, hash, size}
    }

    pub fn for_path_and_uri(
        path: &PathBuf,
        uri: uri::Http
    ) -> Result<FileInfo, Error> {
        let bytes = {
            use std::io::Read;

            let mut f = File::open(path)?;
            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes)?;
            bytes
        };

        let size = bytes.len();

        let hash = {
            use crate::remote::publication;
            use bytes::Bytes;

            hex::encode(&publication::hash(&Bytes::from(bytes)))
        };

        Ok(FileInfo::new(uri, hash, size))
    }

    pub fn for_uri(
        uri: &uri::Http,
        base_uri: &uri::Http,
        rrdp_base: &PathBuf
    ) -> Result<FileInfo, Error> {
        let base_string = base_uri.to_string();
        let uri_string  = uri.to_string();

        if ! uri_string.as_str().starts_with(base_string.as_str()) {
            Err(Error::NotificationFileError)
        } else {
            let (_, rel) = uri_string.split_at(base_string.len());
            let mut path = rrdp_base.clone();
            path.push(rel);

            FileInfo::for_path_and_uri(&path, uri.clone())
        }
    }

    fn for_path(
        path: &PathBuf,
        base_uri: &uri::Http,
        rrdp_base: &PathBuf
    ) -> Result<FileInfo, Error> {
        let relative = path.strip_prefix(rrdp_base)
            .map_err(|_| Error::UriConfigError)?.to_string_lossy();
        let base_uri = base_uri.to_string();
        let uri = uri::Http::from_string(
            format!("{}{}", base_uri, relative)
        ).map_err(|_| Error::UriConfigError)?;

        FileInfo::for_path_and_uri(path, uri)
    }


}


//------------ NotificationBuilder -------------------------------------------

struct NotificationBuilder {
    serial: Option<usize>,
    session_id: Option<String>,
    snapshot: Option<SnapshotRef>,
    deltas: Vec<DeltaRef>
}

impl NotificationBuilder {
    fn new() -> Self {
        NotificationBuilder {
            serial: None,
            session_id: None,
            snapshot: None,
            deltas: Vec::new()
        }
    }

    fn with_serial(&mut self, serial: usize) {
        self.serial = Some(serial);
    }

    fn with_session_id(&mut self, session_id: String) {
        self.session_id = Some(session_id);
    }

    fn with_snapshot(&mut self, snapshot: SnapshotRef) {
        self.snapshot = Some(snapshot);
    }

    fn with_deltas(&mut self, deltas: Vec<DeltaRef>) {
        self.deltas = deltas;
    }

    fn add_delta(&mut self, delta: DeltaRef) {
        self.deltas.push(delta);
    }

    fn add_delta_to_start(&mut self, delta: DeltaRef) {
        self.deltas.insert(0, delta);
    }

    /// Keeps at least two deltas, and beyond that only if the size is
    /// smaller than the snapshot.
    ///
    /// Note we may add something to exclude old deltas later, if we find
    /// that e.g. access to old deltas is very infrequent and excluding
    /// them would shrink the notification file size.
    fn curate_deltas(&mut self) {
        let size_snapshot = match &self.snapshot {
            Some(snapshot) => snapshot.size,
            None => 0
        };
        let mut total_deltas = 0;
        let mut count = 0;

        self.deltas.retain(|d| {
            count = count + 1;
            total_deltas = total_deltas + d.size;
            count <= 2 || total_deltas < size_snapshot
        })

    }


    /// Builds the notification, panics if any of the options are not set.
    /// This can only happen if there is a bug.
    fn build(mut self) -> Notification {
        self.curate_deltas();
        Notification {
            serial: self.serial.unwrap(),
            session_id: self.session_id.unwrap(),
            snapshot: self.snapshot.unwrap(),
            deltas: self.deltas
        }
    }
}


//------------ Error ---------------------------------------------------------

#[derive(Debug, Fail)]
pub enum Error {
    #[fail(display="{}", _0)]
    IoError(io::Error),

    #[fail(display="{}", _0)]
    RecursorError(RecursorError),

    #[fail(display="{}", _0)]
    UriError(uri::Error),

    #[fail(display="File already exists for uri (use update!): {}", _0)]
    ObjectAlreadyPresent(uri::Rsync),

    #[fail(display="Np file present for uri: {}", _0)]
    NoObjectPresent(uri::Rsync),

    #[fail(display="File does not match hash")]
    NoObjectMatchingHash,

    #[fail(display="Publishing outside of base URI is not allowed.")]
    OutsideBaseUri,

    #[fail(display="Issue deriving RRDP URI, check config!")]
    UriConfigError,

    #[fail(display="Error deserializing existing notification.xml")]
    NotificationFileError,

    #[fail(display="{}", _0)]
    XmlReaderErr(XmlReaderErr),

    #[fail(display="{}", _0)]
    AttributesError(AttributesError),

    #[fail(display="{}", _0)]
    ParseIntError(ParseIntError)
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::IoError(e)
    }
}

impl From<uri::Error> for Error {
    fn from(e: uri::Error) -> Self {
        Error::UriError(e)
    }
}

impl From<RecursorError> for Error {
    fn from(e: RecursorError) -> Self {
        Error::RecursorError(e)
    }
}

impl From<XmlReaderErr> for Error {
    fn from(e: XmlReaderErr) -> Self {
        Error::XmlReaderErr(e)
    }
}

impl From<AttributesError> for Error {
    fn from(e: AttributesError) -> Self {
        Error::AttributesError(e)
    }
}

impl From<ParseIntError> for Error {
    fn from(e: ParseIntError) -> Self {
        Error::ParseIntError(e)
    }
}


//------------ Tests ---------------------------------------------------------

#[cfg(test)]
mod tests {

    use super::*;
    use bytes::Bytes;
    use crate::daemon::repo::Notification;
    use crate::util::file::CurrentFile;
    use crate::util::test;

    #[test]
    fn should_publish() {
        test::test_with_tmp_dir(|d| {
            let rrdp_base_uri = test::http_uri("http://localhost:3000/repo/");
            let mut repo = Repository::new(&rrdp_base_uri, &d).unwrap() ;

            // Publish a file
            let rsync_for_alice =
                test::rsync_uri("rsync://host:10873/module/alice");
            let file = CurrentFile::new(
                test::rsync_uri("rsync://host:10873/module/alice/file.txt"),
                Bytes::from("example content")
            );

            let mut builder = PublishQuery::build();
            builder.add(file.clone().as_publish());
            let message = builder.build_message();
            let publish = message.as_query().unwrap().as_publish().unwrap();

            repo.publish(&publish, &rsync_for_alice).unwrap();

            // Now publish an update a bunch of times
            // (overwrite file with same file, strictly speaking allowed,
            // and convenient here)

            let file_update = file.clone();

            let mut builder = PublishQuery::build();
            builder.add(file_update.clone().as_update(file.content()));
            let message = builder.build_message();
            let update = message.as_query().unwrap().as_publish().unwrap();
            repo.publish(&update, &rsync_for_alice).unwrap();
            repo.publish(&update, &rsync_for_alice).unwrap();
            repo.publish(&update, &rsync_for_alice).unwrap();
            repo.publish(&update, &rsync_for_alice).unwrap();
            repo.publish(&update, &rsync_for_alice).unwrap();

            // Now we expect a notification file with serial 6, which only
            // includes deltas for 5 and 6, because more deltas would
            // exceed the size of the snapshot.

            let mut rrdp_disk_path = d.clone();
            rrdp_disk_path.push("rrdp");

            let mut notification_disk_path = rrdp_disk_path.clone();
            notification_disk_path.push("notification.xml");

            match Notification::derive(
                &notification_disk_path,
                &rrdp_base_uri,
                &rrdp_disk_path
            ) {
                Some(notification) => {
                    let expected_serial: usize = 6;
                    let expected_prev: usize = 5;
                    assert_eq!(notification.serial(), &expected_serial);

                    let deltas = notification.deltas();
                    assert_eq!(2, deltas.len());

                    assert!(
                        deltas.iter().find(|d| {
                            d.serial() == &expected_serial}
                        ).is_some()
                    );

                    assert!(
                        deltas.iter().find(|d| {
                            d.serial() == &expected_prev}
                        ).is_some()
                    );
                },
                None => panic!("Should have derived notification"),
            }
        });
    }

    #[test]
    fn should_store_list_withdraw_files() {
        test::test_with_tmp_dir(|d| {
            let mut file_store = FileStore { base_dir: d };

            // Using a port here to make sure that it works in mapping
            // the rsync URI to and from disk.
            let base_uri = test::rsync_uri
                ("rsync://host:10873/module/alice/");

            // Publish a file
            let file = CurrentFile::new(
                test::rsync_uri("rsync://host:10873/module/alice/file.txt"),
                Bytes::from("example content")
            );

            let mut builder = PublishQuery::build();
            builder.add(file.clone().as_publish());
            let message = builder.build_message();
            let publish = message.as_query().unwrap().as_publish().unwrap();

            file_store.publish(&publish, &base_uri).unwrap();

            // See that it's the only one listed
            let files = file_store.list(&base_uri).unwrap();
            assert_eq!(1, files.len());
            assert!(files.contains(&file));

            // Update a file
            let file_update = CurrentFile::new(
                file.uri().clone(),
                Bytes::from("example updated content")
            );

            let mut builder = PublishQuery::build();
            builder.add(file_update.clone().as_update(file.content()));
            let message = builder.build_message();
            let publish = message.as_query().unwrap().as_publish().unwrap();
            file_store.publish(&publish, &base_uri).unwrap();

            // See that it's the only one listed
            let files = file_store.list(&base_uri).unwrap();
            assert_eq!(1, files.len());
            assert!(files.contains(&file_update));

            // Withdraw a file
            let mut builder = PublishQuery::build();
            builder.add(file_update.as_withdraw());
            let message = builder.build_message();
            let publish = message.as_query().unwrap().as_publish().unwrap();
            file_store.publish(&publish, &base_uri).unwrap();

            // See that there are no files listed
            let files = file_store.list(&base_uri).unwrap();
            assert_eq!(0, files.len());
        });
    }

    #[test]
    fn should_not_allow_publishing_or_withdrawing_outside_of_base() {
        test::test_with_tmp_dir(|d| {
            let mut file_store = FileStore { base_dir: d };

            // Using a port here to make sure that it works in mapping
            // the rsync URI to and from disk.
            let base_uri = test::rsync_uri
                ("rsync://host:10873/module/alice/");

            // Publish a file
            let file = CurrentFile::new(
                test::rsync_uri("rsync://host:10873/module/bob/file.txt"),
                Bytes::from("example content")
            );

            let mut builder = PublishQuery::build();
            builder.add(file.clone().as_publish());
            let message = builder.build_message();
            let publish = message.as_query().unwrap().as_publish().unwrap();

            match file_store.publish(&publish, &base_uri) {
                Err(Error::OutsideBaseUri) => {},
                _ => { panic!("Expected Error::OutsideBaseUri") }
            }
        });
    }
}


