// The runtime's files (gex/...), each by its own `new URL('./gex/...',
// import.meta.url)`, so a page's bundler takes every one in and names it
// where it put it. page/runtime/build.sh writes the list into the package;
// in this tree there is none, and the files are under `baseUrl`.
export default null;
