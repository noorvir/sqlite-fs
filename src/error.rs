use minfuse::FsError;

#[derive(Debug)]
pub enum Error {
    Fs(FsError),
    Sql(rusqlite::Error),
    Json(serde_json::Error),
    Yaml(serde_yaml::Error),
    Utf8(std::str::Utf8Error),
    Io(std::io::Error),
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

pub(crate) fn is_semantic_sql_error(err: &Error) -> bool {
    matches!(
        err,
        Error::Sql(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::ConstraintViolation
                || code.code == rusqlite::ErrorCode::TypeMismatch
    )
}

pub(crate) fn to_fs_error(err: Error) -> FsError {
    match err {
        Error::Fs(err) => err,
        Error::Yaml(_) | Error::Utf8(_) => FsError::InvalidInput,
        Error::Sql(_) | Error::Json(_) => FsError::Io,
        Error::Io(err) => io_to_fs_error(err),
    }
}

fn io_to_fs_error(err: std::io::Error) -> FsError {
    match err.kind() {
        std::io::ErrorKind::NotFound => FsError::NotFound,
        std::io::ErrorKind::AlreadyExists => FsError::Exists,
        std::io::ErrorKind::PermissionDenied => FsError::PermissionDenied,
        std::io::ErrorKind::InvalidInput => FsError::InvalidInput,
        _ => FsError::Io,
    }
}

impl From<FsError> for Error {
    fn from(value: FsError) -> Self {
        Error::Fs(value)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Error::Sql(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Error::Json(value)
    }
}

impl From<serde_yaml::Error> for Error {
    fn from(value: serde_yaml::Error) -> Self {
        Error::Yaml(value)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(value: std::str::Utf8Error) -> Self {
        Error::Utf8(value)
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::Io(value)
    }
}
