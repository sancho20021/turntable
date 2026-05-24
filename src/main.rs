mod audio_engine;
mod deck;
mod decoder;
mod just_decode;
mod just_play;
mod pitch_control;
mod player_tester;
mod read_touchpad;
mod scratch;
mod stereo_frame;
mod touchpad_state;

fn main() {
    // read_touchpad::main();
    // pitch_control::play_with_pitch().unwrap();
    scratch::run();
}
