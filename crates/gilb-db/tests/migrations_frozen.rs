//! Applied migrations are immutable — this test is the enforcement.
//!
//! sqlx stores a checksum of every migration it has run. Change a shipped
//! file by so much as a comment and every existing database refuses to open
//! ("migration N was previously applied but has been modified"), which sends
//! the app down the archive-and-start-fresh path: the user's history is
//! renamed aside and the app starts empty. It is a one-line edit with a
//! whole-install blast radius, and it reads as harmless in review — which is
//! exactly why a human rule is not enough.
//!
//! **Changing a migration is never the fix.** Add `000N+1` instead; correct
//! stale prose in code or `help.md`, never in an applied `.sql`.
//!
//! When you legitimately add a migration, add its hash here.

use std::collections::BTreeMap;

/// SHA-256 of every migration that has shipped. Never edit an existing line.
const FROZEN: &[(&str, &str)] = &[
    (
        "0001_init.sql",
        "2940af1d79ce2257d5748f82272f54223ab27e64c82466f026a1792b10cb05f9",
    ),
    (
        "0002_browser_url.sql",
        "aae145c63af2c495d716ede7370289eef7c492fcd83b0a98d321bbf63ea61676",
    ),
    (
        "0003_tree_snapshots_browser_url.sql",
        "f207420361e738da5d021716b2742192e5cdea3d62f26cdba0a9e6d64a988abe",
    ),
    (
        "0004_meetings.sql",
        "9729e428be05294e2cd7203cae944faf9ab675c640a38fe9be332bef152bd4d2",
    ),
    (
        "0005_meeting_transcripts.sql",
        "ac78dff3b6d055c0826c00a7ebe5c6d91c9aab6156f4081e5d586f125466c749",
    ),
];

#[test]
fn shipped_migrations_are_unchanged() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
    let on_disk: BTreeMap<String, String> = std::fs::read_dir(dir)
        .expect("migrations directory")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "sql"))
        .map(|e| {
            let bytes = std::fs::read(e.path()).expect("read migration");
            (e.file_name().to_string_lossy().into_owned(), sha256(&bytes))
        })
        .collect();

    for (name, expected) in FROZEN {
        let actual = on_disk.get(*name).unwrap_or_else(|| {
            panic!("{name} is gone — a shipped migration cannot be deleted either")
        });
        assert_eq!(
            actual, expected,
            "\n{name} has been modified.\n\
             Every database that already ran it will refuse to open and be \n\
             archived away. Revert this file and add a new migration instead.\n"
        );
    }
}

/// Minimal SHA-256 — a dev-dependency for one hash in one test is not worth it.
fn sha256(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let b = &chunk[i * 4..i * 4 + 4];
            *word = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    h.iter().map(|w| format!("{w:08x}")).collect()
}
