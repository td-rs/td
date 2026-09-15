//! Port of `td/utils/emoji.{h,cpp}` — the Unicode emoji table and the
//! emoji-shape helpers over it. The C++ packs the 2334-emoji table as
//! base64url(zlib(space-separated emojis)) and decodes it into a
//! function-local static hash set at first use; the same packed blob is
//! embedded verbatim and the table is built once in a [`OnceLock`].
//!
//! The packed stream is zlib-format (`78 9c` header), not gzip: td's
//! `gzdecode` runs `inflateInit2(MAX_WBITS + 32)` which auto-detects both,
//! and so does this port's [`crate::gzip::gzdecode`].

use std::collections::HashSet;
use std::sync::OnceLock;

use base64::Engine as _;

use crate::gzip::gzdecode;

/// `MAX_EMOJI_LENGTH` (emoji.cpp:10).
const MAX_EMOJI_LENGTH: usize = 28;

/// The packed emoji table — byte-identical to the C++ `Slice packed_emojis`
/// (emoji.cpp:14-112): base64url without padding of a zlib stream of the
/// 2334 space-separated emojis.
const PACKED_EMOJIS: &str = concat!(
    "eJxtmlly40iWRbdCs_rrr56H3WVmzBHE4A6AxCCFqiodcBFOKShSComawqyWglpAl1lvoP0wcdPK2vrDnY8WF453z_NJ",
    "Ck3LdjEtu8WUbWIbYwuxbWPbxXZYTOuX2F4XU13GVi2m5mNsn2L7HNuX2JaxJbGlsWWx5bGZ2GxsRWw_FlMbP9s_xRbH",
    "b-P4bRyzjWN2b2N7F1t8tovjd_Hfu31s8b3dXWyPsUXtWdScxfedxfedXS6m8_jO8_jMeRz3PD53HvM6Xy-mr_Hz6yo2",
    "4pj_1_jubfS2PVtMVzGvq2rxt4uf3sbuTRa79ymdobN0BV1Jh-79im5NV9M1dG3sPv5Md4q-0e3pDnS3dHd03-ke6I50",
    "j7H7lNPxtuVPdIyy_IXuDR1ZLd_Rvaf7QPeR7hPdZ7ovdEu6hI7sl_hYMvLyNDI-lvhY4mOJjyU-lvhY4mOJjyXZx6rH",
    "7ozunO4r3QXdH-mu6K7pcLnc0d3Q4XeJ3yV-l_hd3tPhd4nfJX6XT3TPdC90r3Q_YpdgP8F-gv0E-wn2E-wn2E-wn2A_",
    "wX6C_QT7CfYT7CfYT7CfYD_BfoL9BPsJ9hPsJ9hPsJ9gP8F-gv0E-wn2E-wn2E-wn2A_-RPdn-l-pXN0Pd1A5-ku6TZ0",
    "I12g29KBLgFdAroEdAnoEtAloEtAl4AuYaok8Evgl8AvgV8CvwR-CegS0KWgS0GXgi4FXQq6FHQp6FLQpaBLQZeCLgVd",
    "CroUdCnoUtCloEtBl4IuBV2K_RT7KfZT7KfYT7GfYj_Ffor9FPsp9lPsp9hPsZ9iP8V-iv0U-yn2U-yn2E-xn2I_xX6K",
    "_RT7KfZT7KfYT5k-KQxSGGQwyGCQwSCDQQaDDAYZDDIYZDDIYJDBIMNvht8Mvxl-M_xm-M3wm-E3w2-G3wy_GX4z_Gb4",
    "zfCb4TfDb4bfDL8ZfjP8ZvjNMJjhKMNRhqMMRxmOMhxlOMpwlOMox1GOoxxHOY5yHOU4ynGU4yjHUY6jnKrmVDWnqjlV",
    "zXGZ4zLHZY7LHJc5CyJnQeQsiJwFkbMgchZEzoLIWRA5CyJnQeQsiJwFkUMoh1AOoRxCOYRyCOUQyiGUQyiHUA6hHEI5",
    "hHII5RDKIZRDKGdG5MyInBmRAyxnRuTMiJwZkcMvh18Ovxx-Ofxy-OXwM6AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1Az",
    "UDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQ",
    "M1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzJ2rMOgs6y6yz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPw",
    "s_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz",
    "8LPws_Cz8LPws_Cz8LPws_Cz8LPwK-BXwK-AXwG_An4F_Ar4FfAr4FfAr4BfAb8CfgX8CvgV8CvgV8CvgF8BvwJ-BfwK",
    "-BXwK-BXwK-AXwG_An4F_Ar4FfAr4FfAr4BfAb8CfgX8CvgV8CvgV8CvgF8BvwJ-BfwK-BXwK-BXwK-AXwG_An4F_Ar4",
    "FfAr4FfAr4BfAb8CfgXUSqiVUCuhVkKthFoJtRJqJdRKqJVQK6FWQq2EWgm1Emol1EqolVAroVZCrYRaCbUSaiXUSqiV",
    "UCuhVkKthFoJtRJqJdRKqJVQK6FWQq2EWgm1Emol1EqolVAroVZCrYRaCbUSaiXUSqiVUCuhVkKthFoJtRJqJdRKqJVQ",
    "K6FWQq2EWsmsq7BfYb_CfoX9CucVziucVzivcF7hvMJ5hfMK5xXOK5xXOK9wXuG8wnmF8wrnFc4rnFc4r3Be4bzCeYXz",
    "ivxWvGPFO1ZI1qS7Jt016a5ZJGvKXVPumnLXlLum3DXlril3Tblryl1T7ppy15S7xm-N3xq_NX5ryl1jusZ0jeka0zWm",
    "axKqSajGdI3pGtM1pmtM15iuMV1jusZ0jeka0zWma0zXOKoxXWO6xnRNuWvKXVPumnLXlLum3DXlril3Tblryl1T7ppy",
    "15S7ptw15a4pd025a8pdg7OGXw2_Gn41_Gr4NfBr4NfAr4FfA78Gfg38Gvg18Gvg18CvgV8DvwZ-Dfwa-LWM1zJey3gt",
    "47WM1zJey3gt47WM1zJey3gt47WM1zJey3jtaTzq0VKPlnq01KOlHi31aKlHSz1a6tFSj5Z6tNSjpR4t9WipR0s9WurR",
    "Uo-WerTUo6UeLfVoqUdLPVrq0VKPlnq01KOlHi31aKlHSz1a6tFSj5Z6tNSjpR4t9WipR0s9WurRUo-WerTUo6UeLfVo",
    "qUdLPTr4dfDr4NfBr4NfB78OQh1IOpB0IOkA0QGiA0SH6Q7THaY7THdk35F9R2odqXWk1pFaR2odqXWk1pFaR2oXsLqA",
    "1QWsLmB1AasLWF3A6gJWF7C6gNUFrC542wVcHOk6CuooqKOgjuwd2TuydxTUUVCHD4cPhw9HQR0FdRTUUVCHN4c3hzeH",
    "N0eSjiQdSTqSdCTpSNKRpCNJR5KOJB1JOpJ0IHEU1FFQR0HdKXEK6iioo6AOag5qDmoOag5qDmoOag5gjlo6aumoZU8t",
    "e2rZU8ueWvbUsqeWPcugZxn0LIOeZdCzDHqo9VDrodZDrYdaD7Ueaj3Ueqj1UOuh1kOth1oPtR5qPdR6qPVQ66HWQ62H",
    "Wg-1Hmo91Hqo9VDrodZDrYdaD7Ueaj3Ueqj1UOuh1kOth1oPtR5qPdR6qPVQ66HWQ62HWg-1Hmo9c60HXQ-6HnQ96AbQ",
    "DaAbQDeAbgDdALqBbWSA3wC_AX4D_Ab4DfAb4DfAb4DfAL8BfgP8BvgN8BvgN8BvgN8AvwF-A_wG-A3wG-A3wG-A3wC_",
    "AX4D_Ab4DfAb4DfAb4DfAL8BfgP8BvgN8BvgN8BvgN8AvwF-A_wG-A3wG-A3wG-A3wC_AX4D_Ab4DfAb4DfAb4Cfh5-H",
    "n4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4ef",
    "h5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh9-G8TaMt2G8DeNtGG_D",
    "eBvG2zDehvE2jLdhvA3jbRhvpAAjBRgpwEgBRgowUoCRAowUYKQAIwUY4TzCeYTzCOcRziOcRziPcB7hPMJ5hPMI5xHO",
    "I5xHOI9wHuE8wnmE8wjnEc4jnEc4j3Ae4TzCeYTzCOcRziOcRziPcB7hPMJlhMsIlxEuI1xGuIxwGeEywmWEywiXES7j",
    "iQucRziPcA5gCmAKYApgCmAKYApgCkzCAJwAnACcAJwAnACcAJwAnACcAJwAnACcAJwAnACSAJIAkgCSAJIAkgCSAJIA",
    "kgCSAJKA6YDpgOmA6YDpgOmA6YDp8LD4y2bxl-vF9NPzYvr502J6--fYjovpfRlbFdsqtnVsdWxNbJvYxsW0vFxMWRbb",
    "VWzx-exbbDex7WN7iC2OkT0upviD4LSK-lWI7ftiWv8U21Ns8X3xJjzFi_AU78FTvAZP8RY8xXvtFC-zU7ytTvFiOsXb",
    "6BTvoVO8hk7x7jnFu-YU75ZTvFpO8cY4xQvjFO-LU7wVTvE-ODUXscVnmqhv-tjiM03MtYnvbF4XU7yTTfE2NsXL2BTv",
    "YlO8ik3xEjbF69cUb19TvG1N8To1xVvSFC9JU_cxtugz3l6mrogt-u_iv3X8221s97HFPOJtZDqL7z-L-rPI8WwZWxJb",
    "fPYsvvMsvvMsvu8sejr7U2yH2OLz59Hz-YfYYr7nbjF9jf8WD-8pnt3T9l1s72P7sPjrT7vYXhZ__byOjYnyjhK_o8Tv",
    "mJvvmJsfmZEfT794ZrYsmSNL5siSObJkjiyZI0vmyJJls2TZLFk2y9MvXU-_V2RapkzLlBelzMiUGZmySFNmZMaum7Hr",
    "Zuy6GRM-Y15nzOuMATIGyJjXGaNkjJIxSsa8zlj0GeNlp_GYoBlzMzv9lgsz5vR7hNOPemw2Fbt9xYqucF7xRMWUrnis",
    "IvuKZyvWccU6XrFbrXhsRbor0l2R7opVuYLLChorIK5YJ6vTj2tAXLOs1yzrNat3zepds3rXZL8m-zXZr-G8ZkGuGWpN",
    "fmtSW58upry345UdhDq2hI7F3PFYx2LueLajUB0JdVSmw2B3WPxhevPr4g___ZDx-Q-nbv7yj6du_vJPp27-8s-nbv7y",
    "L6du_vKvp27-8m-nbv7y76du_vIfp27-8p-nbv7yX6du_vK3iw9DbJcKNgpGBUHBVsG1ghsFewW3Cr4ruFfwoOCo4FHB",
    "s4IXBUycD54vCryCjYJRQVCwVXCl4FrBNwU3CvYKDgpuFXxXcK_gQcFRwZOCZwWvCk7JXyr5S-G9VPKXyvlSOV8q50vl",
    "TLBTcKNgr-Cg4FbBnYLvCu4VPCp4UvCs4EXBq4JT8hvh3SjDjRhulNhG-WyUxkaPj_I-yvuoAUcNOMryqFRHcR7FeVTy",
    "QViC0ghKIyiNoDSCBtwqja3mz1Yl2CqfrWqxVWJbJbbVS7cqwVbv2qoEW5Hfivz279_-oOCo4FHBs4IT-SvZudIrrvSK",
    "Kw14pXGuNM618F7L17V8XSvnaw14rQGvBepaOV_rFdfK-Vrv-qYBv2mcb3r8m7zvpNmJ4U4Md2K40-M7pbH7-8fvFTwr",
    "eFVwmlE3KuWNSnkj7zd6xY0Y3mjAG9m5kZ0boSN4UnB6116v2GvkvajuZXCv2bKX072c7vX2vcjvZXkvy3uh28v7XiXY",
    "K-e9ct4r571y3ivnvUARvCj43cWJ2EF2DrJzkIuDXBzk4iCGByV_UKoHpXpQhgflc9C7buX0Ti-907vu9K47vetOxO5E",
    "7E4vvfv7cQ4K7hU8KDgqeFZw8v5db7_X2-_l4l6P3yv5ez3-oKcIvIJLBRsFo4KtgisF1wq-KdgpuFGwV3BQcKvgXsGD",
    "gqOCJwUvCl4VnMgflfxROR-V81Hkj8r5qJyPSvWoVI9K9ahUj0r1qFSPSvWoDI_K8CiYRyX2qMQe9fZHjfyokR9l-VG-",
    "HvX4kx5_kq8nleBJAz6J_JMGfFJxn-X9Wa94kdNXjfMqFz_0rh_K8MdsJ7v9nz_-7GP7c2x9bNfz9x__9x8Psf0a2-3_",
    "94_3sW1n0Q9-HDr9h_lvn8_z58v8-Tp__uBHkd90p8_n-fNl_nydP3_w50O_6U6fz_Pny_z5On-i-zLrvsy6L7Puy6z7",
    "MuuWs24565azbjnrlrMumXXJrEtmXTLrkvmPANJ3818InIJnBS8KXhWcyP2iPyf4RX9S8Iv-rOAX_WnBLxK_kfiNxG8k",
    "fiPxG4nfSvxW4rcSv5X4rcQfJP4g8QeJP0j8QeLPEn-W-LPEnyX-LPEXib9I_EXiLxJ_kXgp8VLipcRLiZez2AidEToj",
    "dEbojNAZoTNCZ4TOCJ0ROvNe4vcSv5f4vcTvJRY6I3RG6IzQGaEzHyX-KPFHiT9K_FHiTxJ_kviTxJ8k_iSximJUFKOi",
    "GBXFqChGRTEqilFRjIpiVBSjohgVxagoRkUxvxclkTiROJE4kVgrxaQSpxKnEqcSpxJnEmcSZxJnEmcS5xLnEucS5xLn",
    "Eg8SDxIPEg8SDxJ7ib3EXmIvsZf4UuJLiS8lvpT4UuKNxBuJNxJvJN5IHCQOEgeJg8RB4q3EW4m3Em8l3kp8JfGVxFcS",
    "X0l8JfG1xNcSX0t8LfG1xDuJdxLvJN5JvJP4RuIbiW8kvpH4RuK9xHuJ9xLvJd5LfJD4IPFB4oPEB4lvJb6V-FbiW4lv",
    "Jb6T-E7iO4nvJL6T-LvE3yX-LvF3ib9LfC_xvcT3Et9LfC_xg8QPEj9I_CDxg8TPEj9L_Czxs8TPs9j-PItPwbOCFwWv",
    "Ck5i7c9W-7PV_my1P1vtz1b7s9X-bLU_W-3PVvuz1QlrdcJanbBWJ6zVCWu1mVtt5labudVmbrWZW23mVpu51WZutZlb",
    "beZWO5LVjmS1I1ntSFY7kjUSG4mNxEZiI_Eo8SjxKPEo8TiLK03RSlO00hStNEUrTdFKU7TSFK00RStN0UpTtHqU-FHi",
    "R4kfJX6cxat8Fp-CZwUvCl4VnMSVxJXElcSVxJXEK4lXEq8kXkm8msWN5kajudFobjSaG43mRqO50WhuNJobjeZGo7nR",
    "aG40mhuN5kajudFobjQ6YRudsI1O2EYnbKMTttEJ2-iEbXTCNjphG52wjU7YRidsoxO20Qnb6IRtdMI2OmEbnbCNTthG",
    "J2yj-dxoPjeaz43mc6P53P46i0_Bs4IXBa8KTmJN0VZTtNUUbTVFW03RVlO01RRtNUVbTdFWU7TVLtpqF221i7baRVvt",
    "ot1Ps_gUPCt4UfCq4CRWUToVpVNROhWlU1GcxE5iJ7GT2P0uFmcnzk6cnTg7cXa1xLXEtcS1xLXEjcSNxI3EjcSNxK3E",
    "rcStxK3ErcSdxJ3EncSdxJ3EZxKfSXwm8ZnEZxKfS3wu8bnE5xKfS_xV4q8Sf5X4q8RfJb6Q-ELiC4kvJL6QWJdAp0ug",
    "0yXQ6RLodAl0O4l3Eu8k3km8k1g3GaebjNNNxukm43STcbrJON1knG4yTjcZp5uM003G6SbjdJNxusk43WSc1qDTGnRa",
    "g05r0GkNOq1BpzXotAad1qDTGnRag05r0GkNOq1BpzXodJNxusk43WScbjJONxmnm4zTTcbpJuN0k3G6ybijxEeJjxIf",
    "JT5K_CLxi8QvEr9I_CLxq8SvEr9K_Crx6yzuZbCXwV4GexnsZXAQ50GcB3EexHkQ50GcB3EexHkQ50GcB6EbhG4QukHo",
    "BqEbhG4QukHoBqEbhG54kvhJ4ieJnyR-msVep5XXaeV1WnmdVl6nlddp5XVaeZ1WXqeV12nltYt67aJeu6jXLuq1i3rd",
    "vrxuX163L6_bl9fty1uJrcRWYiuxlbiQuJC4kLiQuJC4lLiUuJS4lLiUWHckrzuS1x3J647kdUfyuiN53ZG87khedySv",
    "O5JfS7yWeC3xWuK1xDqAvA4grwPI6wDyOoC8DiCvA8jrAPI6gLwOIK8DyOsA8jqAvA4grwPI6wDyOoC8DiCvA8jrAPI6",
    "gLwOIK8DyOsA8jqAvA4grwPI6wDyOoC8DqCgn1OCfk4J-jkl6OeUoJ9Tgn4FF_QruKBfwQX9Ci7oV3BBF9egi2vQxTXo",
    "4hp0cQ07iXcS7yTeSbyTWAdQ0AEUdAAFHUBBB1DQARR0AAUdQEEHUNABFHQABR1AQQdQ0AEUdAAFHUBBB1DQARR0AAUd",
    "QEEbY9DGGLQxBm2MQRtj0MYYtDEGbYxBG2PQxhi0Pwftz0H7c9D-HLQ_B-2iQbto0C4atIuG33bR_wUTXk_W",
);

/// `EMOJI_COUNT` (emoji.cpp:113).
const EMOJI_COUNT: usize = 2334;

/// The emoji table — the C++ function-local `FlatHashSet<Slice, SliceHash>`
/// (emoji.cpp:12-124), built once from the packed blob. The decoded blob is
/// leaked so the set entries are `'static`.
fn emoji_table() -> &'static HashSet<&'static [u8]> {
    static TABLE: OnceLock<HashSet<&'static [u8]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let packed = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(PACKED_EMOJIS.as_bytes())
            .expect("the packed emoji table is valid base64url");
        let decoded = gzdecode(&packed);
        let all_emojis_str: &'static [u8] =
            Box::leak(decoded.as_slice().to_vec().into_boxed_slice());

        // The C++ parse loop (emoji.cpp:115-122): every entry starts on a
        // non-space byte, is at least two bytes long, ends at the next space
        // (the blob ends with a trailing space) and is at most
        // MAX_EMOJI_LENGTH bytes — the CHECKs stay as asserts.
        let mut all_emojis = HashSet::with_capacity(EMOJI_COUNT);
        let mut i = 0;
        while i < all_emojis_str.len() {
            assert!(all_emojis_str[i] != b' ');
            assert!(all_emojis_str[i + 1] != b' ');
            let mut j = i + 2;
            while j < all_emojis_str.len() && all_emojis_str[j] != b' ' {
                j += 1;
            }
            assert!(j < all_emojis_str.len());
            all_emojis.insert(&all_emojis_str[i..j]);
            assert!(j - i <= MAX_EMOJI_LENGTH);
            i = j + 1;
        }
        assert_eq!(all_emojis.len(), EMOJI_COUNT);
        all_emojis
    })
}

/// `is_emoji_element` (emoji.cpp:115-139) — a single table element, with one
/// optional trailing variation selector U+FE0F; two consecutive selectors
/// are rejected.
fn is_emoji_element(str: &[u8]) -> bool {
    let emojis = emoji_table();
    let len = str.len();
    if len > MAX_EMOJI_LENGTH + 3 {
        return false;
    }
    if emojis.contains(str) {
        return true;
    }
    if len <= 3 || str[len - 3] != 0xEF || str[len - 2] != 0xB8 || str[len - 1] != 0x8F {
        return false;
    }
    if len >= 6 && str[len - 6] == 0xEF && str[len - 5] == 0xB8 && str[len - 4] == 0x8F {
        return false;
    }
    emojis.contains(&str[..len - 3])
}

/// `is_emoji` (emoji.h:14, emoji.cpp:141-159) — checks whether the string is
/// an emoji; variation selectors are ignored. Zero-width-joiner sequences
/// are valid when every joined element is an emoji.
pub fn is_emoji(str: &str) -> bool {
    let bytes = str.as_bytes();
    // A 0xE2 byte can only be a UTF-8 lead byte, so the found position and
    // every zero-width joiner position are char boundaries.
    let limit = (MAX_EMOJI_LENGTH + 4).min(bytes.len());
    let Some(mut i) = bytes[..limit].iter().position(|&b| b == 0xE2) else {
        return is_emoji_element(bytes);
    };
    let mut start_pos = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == 0xE2 && bytes[i + 1] == 0x80 && bytes[i + 2] == 0x8D {
            // zero-width joiner \u200D
            if !is_emoji_element(&bytes[start_pos..i]) {
                return false;
            }
            start_pos = i + 3;
            i += 2;
        }
        i += 1;
    }
    is_emoji_element(&bytes[start_pos..])
}

/// `get_fitzpatrick_modifier` (emoji.h:17, emoji.cpp:161-171) — checks
/// whether the emoji ends on a Fitzpatrick modifier and returns its number
/// (2-6) or 0.
pub fn get_fitzpatrick_modifier(emoji: &str) -> i32 {
    let bytes = emoji.as_bytes();
    if bytes.len() < 4
        || bytes[bytes.len() - 4] != 0xF0
        || bytes[bytes.len() - 3] != 0x9F
        || bytes[bytes.len() - 2] != 0x8F
    {
        return 0;
    }
    let c = bytes[bytes.len() - 1];
    if !(0xBB..=0xBF).contains(&c) {
        return 0;
    }
    i32::from(c - 0xBB) + 2
}

/// `remove_fitzpatrick_modifier` (emoji.h:19, emoji.cpp:173-178) — removes
/// all Fitzpatrick modifiers from the end of the string. Every removed
/// 4-byte suffix is a complete character, so the slices stay on char
/// boundaries.
pub fn remove_fitzpatrick_modifier(emoji: &str) -> &str {
    let mut emoji = emoji;
    while get_fitzpatrick_modifier(emoji) != 0 {
        emoji = &emoji[..emoji.len() - 4];
    }
    emoji
}

/// `remove_emoji_modifiers` (emoji.h:22, emoji.cpp:180-184).
pub fn remove_emoji_modifiers(emoji: &str, remove_selectors: bool) -> String {
    let mut result = emoji.to_owned();
    remove_emoji_modifiers_in_place(&mut result, remove_selectors);
    result
}

/// `remove_emoji_modifiers_in_place` (emoji.h:24, emoji.cpp:186-215) —
/// removes all emoji modifiers from the string in place.
pub fn remove_emoji_modifiers_in_place(emoji: &mut String, remove_selectors: bool) {
    const MODIFIERS: [&[u8]; 8] = [
        b"\xEF\xB8\x8F",             // variation selector-16 U+FE0F
        b"\xE2\x80\x8D\xE2\x99\x80", // ZWJ + female sign
        b"\xE2\x80\x8D\xE2\x99\x82", // ZWJ + male sign
        b"\xF0\x9F\x8F\xBB",         // fitzpatrick type-1-2 U+1F3FB
        b"\xF0\x9F\x8F\xBC",         // fitzpatrick type-3 U+1F3FC
        b"\xF0\x9F\x8F\xBD",         // fitzpatrick type-4 U+1F3FD
        b"\xF0\x9F\x8F\xBE",         // fitzpatrick type-5 U+1F3FE
        b"\xF0\x9F\x8F\xBF",         // fitzpatrick type-6 U+1F3FF
    ];
    let start_index = if remove_selectors { 0 } else { 1 };
    // The C++ compacts the byte string in place. Every removed modifier is a
    // complete UTF-8 sequence and the kept bytes are untouched, so the
    // compacted bytes are still valid UTF-8.
    let bytes = unsafe { emoji.as_mut_vec() };
    let mut j = 0;
    let mut i = 0;
    while i < bytes.len() {
        let mut is_found = false;
        for modifier in MODIFIERS.iter().skip(start_index) {
            if i + modifier.len() <= bytes.len() && &bytes[i..i + modifier.len()] == *modifier {
                // skip the modifier
                i += modifier.len();
                is_found = true;
                break;
            }
        }
        if !is_found {
            bytes[j] = bytes[i];
            j += 1;
            i += 1;
        }
    }
    if j != 0 {
        // The C++ guards the resize with `j != 0` (emoji.cpp:212-214): an
        // all-modifier string is left UNCHANGED instead of becoming empty.
        bytes.truncate(j);
    }
}

/// `remove_emoji_selectors` (emoji.h:28, emoji.cpp:217-232) — removes all
/// emoji selectors from the string if it is an emoji.
pub fn remove_emoji_selectors(emoji: &str) -> String {
    if !is_emoji(emoji) {
        return emoji.to_owned();
    }
    let bytes = emoji.as_bytes();
    let mut result: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 3 <= bytes.len() && bytes[i] == 0xEF && bytes[i + 1] == 0xB8 && bytes[i + 2] == 0x8F
        {
            // skip \uFE0F
            i += 2;
        } else {
            result.push(bytes[i]);
        }
        i += 1;
    }
    let str = String::from_utf8(result).expect("stripping complete characters keeps UTF-8");
    // C++ CHECK(is_emoji(str)) (emoji.cpp:230).
    assert!(is_emoji(&str));
    str
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_matches_the_cpp_snapshot() {
        let table = emoji_table();
        assert_eq!(table.len(), EMOJI_COUNT);
        // The longest entry saturates MAX_EMOJI_LENGTH (the C++ CHECK at
        // emoji.cpp:120-121 pins it for every entry).
        assert_eq!(table.iter().map(|emoji| emoji.len()).max(), Some(28));
        // The table stores bare elements: the FE0F-suffixed heart is not an
        // entry while the bare heart is — the selector is handled by
        // is_emoji_element's suffix branch.
        assert!(table.contains("❤".as_bytes()));
        assert!(!table.contains("❤\u{FE0F}".as_bytes()));
        // No entry contains a zero-width joiner — the sequences are
        // composed by is_emoji's element split.
        assert!(
            table
                .iter()
                .all(|emoji| { !emoji.windows(3).any(|w| w == [0xE2, 0x80, 0x8D]) })
        );
    }

    #[test]
    fn is_emoji_matches_the_cpp_shape() {
        // Table elements.
        assert!(is_emoji("⌚"));
        assert!(is_emoji("⌛"));
        // Element + one trailing variation selector.
        assert!(is_emoji("⌚\u{FE0F}"));
        // Two consecutive selectors are rejected (emoji.cpp:135-137).
        assert!(!is_emoji("⌚\u{FE0F}\u{FE0F}"));
        // A selector alone is not an emoji.
        assert!(!is_emoji("\u{FE0F}"));
        // Zero-width-joiner sequences: every element must be an emoji.
        assert!(is_emoji("👨\u{200D}👩\u{200D}👦"));
        assert!(is_emoji("👨\u{200D}👩\u{200D}👦\u{FE0F}"));
        assert!(!is_emoji("👨x\u{200D}👩\u{200D}👦"));
        // Non-emoji input.
        assert!(!is_emoji(""));
        assert!(!is_emoji("a"));
        assert!(!is_emoji("⌚⌛"));
        assert!(!is_emoji("⌚⌛⌚⌛⌚⌛⌚⌛⌚⌛"));
        // A Fitzpatrick-suffixed form that is a table element itself.
        assert!(is_emoji("🫸\u{1F3FF}"));
    }

    #[test]
    fn fitzpatrick_helpers_match_the_cpp_shape() {
        // 🫸🏿 = U+1F3B8 U+1F3FF ends on the type-6 modifier.
        assert_eq!(get_fitzpatrick_modifier("🫸\u{1F3FF}"), 6);
        assert_eq!(get_fitzpatrick_modifier("🫸\u{1F3FB}"), 2);
        assert_eq!(get_fitzpatrick_modifier("👍"), 0);
        assert_eq!(get_fitzpatrick_modifier("\u{1F3FF}"), 6);
        assert_eq!(get_fitzpatrick_modifier(""), 0);
        assert_eq!(remove_fitzpatrick_modifier("🫸\u{1F3FF}\u{1F3FB}"), "🫸");
        assert_eq!(remove_fitzpatrick_modifier("👍"), "👍");
    }

    #[test]
    fn modifier_removal_matches_the_cpp_shape() {
        // The selector is removed only when remove_selectors is set.
        assert_eq!(remove_emoji_modifiers("👍\u{FE0F}", true), "👍");
        assert_eq!(remove_emoji_modifiers("👍\u{FE0F}", false), "👍\u{FE0F}");
        // The ZWJ+female-sign modifier carries its joiner and is removed at
        // both start indexes; only the plain selector depends on the flag.
        assert_eq!(
            remove_emoji_modifiers("👋\u{200D}\u{2640}\u{FE0F}", false),
            "👋\u{FE0F}"
        );
        assert_eq!(
            remove_emoji_modifiers("👋\u{200D}\u{2640}\u{FE0F}", true),
            "👋"
        );
        // C++ quirk kept (emoji.cpp:212-214): an all-modifier string is left
        // unchanged because the resize is guarded by j != 0.
        assert_eq!(remove_emoji_modifiers("\u{FE0F}", true), "\u{FE0F}");
        // remove_emoji_selectors strips FE0F from an emoji...
        assert_eq!(remove_emoji_selectors("⌚\u{FE0F}"), "⌚");
        // ...and returns a non-emoji untouched.
        assert_eq!(remove_emoji_selectors("a\u{FE0F}"), "a\u{FE0F}");
    }
}
