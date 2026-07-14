use super::*;

#[test]
fn input_edits_then_take_clears() {
    let mut app = app();
    app.insert_char('h');
    app.insert_char('i');
    app.insert_newline();
    app.insert_char('x');
    app.backspace();
    assert_eq!(app.input, "hi\n");
    assert_eq!(app.take_input(), "hi\n");
    assert!(app.input.is_empty());
    assert_eq!(app.cursor, 0, "take_input resets the cursor");
}

fn typed(text: &str) -> App {
    let mut app = app();
    for c in text.chars() {
        app.insert_char(c);
    }
    app
}

#[test]
fn insert_and_delete_happen_at_the_cursor() {
    let mut app = typed("helo"); // cursor at end
    app.move_left();
    app.move_left();
    app.insert_char('l'); // "hello", between the two spots
    assert_eq!(app.input, "hello");

    app.move_home();
    app.delete(); // forward-delete 'h'
    assert_eq!(app.input, "ello");
    assert_eq!(app.cursor, 0);

    app.move_end();
    app.backspace(); // delete-before 'o'
    assert_eq!(app.input, "ell");
}

#[test]
fn movement_stops_at_bounds_and_respects_utf8() {
    let mut app = app();
    app.move_left(); // at start: no-op, no panic
    app.delete(); // at end of empty: no-op
    let mut app = typed("你好"); // 3 bytes each
    app.move_left();
    assert_eq!(app.cursor, 3, "left lands on a char boundary, not mid-byte");
    app.move_right();
    app.move_right(); // already at end: no-op
    assert_eq!(app.cursor, 6);
    app.backspace();
    assert_eq!(app.input, "你");
}

#[test]
fn home_end_act_on_the_current_logical_line() {
    let mut app = typed("ab\ncd"); // cursor at end, on line "cd"
    app.move_home();
    assert_eq!(app.cursor, 3, "home → start of the current line");
    app.insert_char('Z');
    assert_eq!(app.input, "ab\nZcd");
    app.move_end();
    assert_eq!(app.cursor, app.input.len(), "end → end of the current line");
}

#[test]
fn up_down_move_by_logical_line_clamping_the_column() {
    let mut app = typed("abcd\nxy\nwxyz"); // cursor at end of "wxyz" (col 4)
    app.move_up(); // "xy" is shorter → clamp to its end (col 2)
    assert_eq!(&app.input[..app.cursor], "abcd\nxy");
    app.move_up(); // column is now 2 → col 2 of "abcd"
    assert_eq!(&app.input[..app.cursor], "ab");
    app.move_down(); // back down to "xy", col 2 → its end
    assert_eq!(&app.input[..app.cursor], "abcd\nxy");
    app.move_up();
    app.move_up(); // already on the first line: no-op
    assert_eq!(&app.input[..app.cursor], "ab");
}
