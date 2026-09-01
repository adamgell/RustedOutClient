#![no_main]

#[cfg(not(fuzzing))]
compile_error!("fuzz targets must be built with --cfg fuzzing");

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    rustedoutclient_fuzz::run_rfb_zrle(data);
});
