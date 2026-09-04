// `sqlx::migrate!` embeds the migration directory at compile time, but a
// proc macro cannot emit cargo directives, so nothing tells cargo to rebuild
// when a migration is ADDED. Without this the crate keeps an older set: the
// binary then applies fewer migrations than the repository contains, which
// is the "binary and the database disagree" failure `pool-admin migrate`
// exists to refuse — arrived at through the build rather than the schema.
//
// sqlx documents this build script for the purpose. It cost two confusing
// local failures before it was added.
fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}
