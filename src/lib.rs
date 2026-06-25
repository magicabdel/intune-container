//! intune-container: core library for managing Microsoft Intune in a pure-Rust,
//! rootless container.
//!
//! This crate contains all the logic — container lifecycle, enrollment, browser
//! SSO, backups, health checks — exposed as plain Rust functions. The
//! [`ops`] module is the high-level command surface that both front-ends use:
//!
//! * the command-line tool (any subcommand), and
//! * the Tauri desktop interface (the default).
//!
//! Both call the same [`ops`] functions directly (in-process); neither shells
//! out to the other. The container runs rootless via [`backend`] (user
//! namespaces, no host root): see [`runtime`], [`oci`], and [`provision`].

pub mod backend;
pub mod backup;
pub mod compositor;
pub mod config;
pub mod display;
pub mod doctor;
pub mod lock;
pub mod native_host;
pub mod ops;

/// Pure-Rust OCI image pull + layer extraction (no docker/podman).
pub mod oci;

/// Pure-Rust rootless container runtime (user namespaces, `setns`, no host root).
pub mod runtime;

/// In-Rust provisioning of the rootfs (session profile, keyring, runtime setup).
pub mod provision;
