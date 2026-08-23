/// Resolve a uid through the host's passwd database, falling back to its numeric representation.
pub fn username(uid: u32) -> String {
    std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                let mut fields = line.split(':');
                let name = fields.next()?;
                let found_uid: u32 = fields.nth(1)?.parse().ok()?;
                (found_uid == uid).then(|| name.to_string())
            })
        })
        .unwrap_or_else(|| uid.to_string())
}
