#[macro_export]
macro_rules! println {
    () => {{
        esp_println::println!();
    }};
    ($($arg:tt)*) => {{
        esp_println::println!($($arg)*);
        $crate::println::log(&alloc::format!($($arg)*));
    }};
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        esp_println::print!($($arg)*);
        $crate::println::log(&alloc::format!($($arg)*));
    }};
}

pub fn log(s: &str) {
    crate::ui::LOG_BUFFER.lock(|buf| {
        let mut buf = buf.borrow_mut();

        if buf.len() > 20 {
            buf.remove(0);
        }

        use crate::alloc::string::ToString;
        buf.push(s.to_string());
    });
}
