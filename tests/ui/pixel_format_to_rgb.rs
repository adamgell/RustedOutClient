use rustedoutclient::vnc::messages::PixelFormat;

fn main() {
    let malformed = PixelFormat {
        bits_per_pixel: 8,
        depth: 8,
        big_endian: false,
        true_colour: true,
        red_max: 0,
        green_max: 7,
        blue_max: 3,
        red_shift: 5,
        green_shift: 2,
        blue_shift: 0,
    };
    let _ = malformed.to_rgb(0);
}
