//! Failpoints: named places on the durable path where a test build can
//! kill the process, fail with an I/O error, panic, stall, or park it until
//! a test script answers. The one registry of every engine's points, and
//! the gates a test drives over FIFOs, which know no engine's types
//! (docs/development.md explains crash injection; docs/configuration.md
//! lists the variables). In a build without the `failpoints` feature, as in
//! every release and image, a point is an inlined `Ok(())`.
