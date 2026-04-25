#![allow(dead_code, unused_variables, unused_imports)]

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "mysql")]
pub mod mysql;

#[cfg(feature = "mssql")]
pub mod mssql;

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "oracle")]
pub mod oracle;
