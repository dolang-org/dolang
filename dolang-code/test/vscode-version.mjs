// The VS Code release both test configurations run against. Pinned, rather
// than left to resolve the latest stable release, so that CI can restore its
// cached copy of this release and never touch the network: an unpinned run
// asks the update server for the latest version even when it already has that
// version downloaded. CI keys that cache on this file, so bumping the version
// downloads the new release once.
export const version = "1.148.0";
