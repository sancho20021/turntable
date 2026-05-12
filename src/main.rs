mod audio_engine;
mod deck;
mod decoder;
mod just_decode;
mod just_play;
mod pitch_control;
mod read_touchpad;
mod stereo_frame;
mod touchpad_state;
mod scratch;

fn main() {
    read_touchpad::main();
}
