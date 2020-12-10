use std::collections::HashMap;
use termcolor::Color;

pub struct ColorPicker<'a> {
    color_map: HashMap<&'a str, usize>,
    colors: Vec<Color>,
    color_index: usize,
}

impl<'a> ColorPicker<'a> {
    pub fn new() -> ColorPicker<'a> {
        ColorPicker {
            color_map: HashMap::new(),
            colors: vec![
                Color::Rgb(2, 63, 165),
                Color::Rgb(125, 135, 185),
                Color::Rgb(187, 119, 132),
                Color::Rgb(142, 6, 59),
                Color::Rgb(74, 111, 227),
                Color::Rgb(133, 149, 225),
                Color::Rgb(181, 187, 227),
                Color::Rgb(230, 175, 185),
                Color::Rgb(224, 123, 145),
                Color::Rgb(211, 63, 106),
                Color::Rgb(17, 198, 56),
                Color::Rgb(141, 213, 147),
                Color::Rgb(240, 185, 141),
                Color::Rgb(239, 151, 8),
                Color::Rgb(15, 207, 192),
                Color::Rgb(156, 222, 214),
                Color::Rgb(247, 156, 212),
            ],
            color_index: 0,
        }
    }

    pub fn get(&mut self, name: &'a str) -> Color {
        if self.color_map.contains_key(name) {
            let index = *self.color_map.get(name).unwrap();
            *self.colors.get(index).unwrap()
        } else {
            self.color_map.insert(name, self.color_index);
            let old_color_index = self.color_index;
            self.color_index = (self.color_index + 1) % self.colors.len();
            *self.colors.get(old_color_index).unwrap()
        }
    }
}
