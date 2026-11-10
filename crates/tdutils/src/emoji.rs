//! Port of `td/utils/emoji.{h,cpp}` — the Unicode emoji table and the
//! emoji-shape helpers over it. The C++ packs the 2370-emoji table as
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
/// 2370 space-separated emojis.
const PACKED_EMOJIS: &str = concat!(
    "eJxtmlly40iWRbdCs_rrr56H3WXGPBCDOwARgxSqrHTARTglUaQUEjWFWS0FtYAus95A-2HipqW19Yc7Hy0uHe-e5xMZ",
    "mpbtYlp2iylbxzbGFmLbxLaNbb-YVi-xvS6muoytWkzNp9g-x_Yltq-xLWNLYktjy2LLYzOx2diK2H4spja-tr_EFsdv",
    "4_htHLONY3bvYnsfW_xsF8fv4r93u9jic7u72B5ji9rTqDmNzzuNzzu9WExn8Zln8TNncdyz-LmzmNfZajF9i6_fTmIj",
    "jvl_i8_eRG-b08V0GfO6rBZ_P__pXezeZrH7kNIZOktX0JV06D6c0K3oarqGro3dp5_pjtE13Y5uT3dLd0f3ne6B7kD3",
    "GLvPOR1PW_5ExyjLN3Rv6chq-Z7uA91Huk90n-m-0H2lW9IldGS_xMeSkZfHkfGxxMcSH0t8LPGxxMcSH0t8LMk-Vj12",
    "p3RndN_ozun-THdJd0WHy-WW7oYOv0v8LvG7xO_yng6_S_wu8bt8onume6F7pfsRuwT7CfYT7CfYT7CfYD_BfoL9BPsJ",
    "9hPsJ9hPsJ9gP8F-gv0E-wn2E-wn2E-wn2A_wX6C_QT7CfYT7CfYT7CfYD_BfoL9BPvJL3R_ofuVztH1dAOdp7ugW9ON",
    "dIFuQwe6BHQJ6BLQJaBLQJeALgFdArqEqZLAL4FfAr8Efgn8EvgloEtAl4IuBV0KuhR0KehS0KWgS0GXgi4FXQq6FHQp",
    "6FLQpaBLQZeCLgVdCroUdCn2U-yn2E-xn2I_xX6K_RT7KfZT7KfYT7GfYj_Ffor9FPsp9lPsp9hPsZ9iP8V-iv0U-yn2",
    "U-yn2E-xn2I_ZfqkMEhhkMEgg0EGgwwGGQwyGGQwyGCQwSCDQQaDDL8ZfjP8ZvjN8JvhN8Nvht8Mvxl-M_xm-M3wm-E3",
    "w2-G3wy_GX4z_Gb4zfCb4TfDYIajDEcZjjIcZTjKcJThKMNRjqMcRzmOchzlOMpxlOMox1GOoxxHOY5yqppT1Zyq5lQ1",
    "x2WOyxyXOS5zXOYsiJwFkbMgchZEzoLIWRA5CyJnQeQsiJwFkbMgchZEDqEcQjmEcgjlEMohlEMoh1AOoRxCOYRyCOUQ",
    "yiGUQyiHUA6hnBmRMyNyZkQOsJwZkTMjcmZEDr8cfjn8cvjl8Mvhl8PPgM5AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1A",
    "zUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDN",
    "QM1AzUDNQM1AzUDNQM1AzUDNQM1AzUDNQM0cqTHrLOgss87Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LP",
    "ws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_Cz8LPws_C",
    "z8LPws_Cz8LPws_Cz8LPws_Cz8KvgF8BvwJ-BfwK-BXwK-BXwK-AXwG_An4F_Ar4FfAr4FfAr4BfAb8CfgX8CvgV8Cvg",
    "V8CvgF8BvwJ-BfwK-BXwK-BXwK-AXwG_An4F_Ar4FfAr4FfAr4BfAb8CfgX8CvgV8CvgV8CvgF8BvwJ-BfwK-BXwK-BX",
    "wK-AXwG_An4F_Ar4FVAroVZCrYRaCbUSaiXUSqiVUCuhVkKthFoJtRJqJdRKqJVQK6FWQq2EWgm1Emol1EqolVAroVZC",
    "rYRaCbUSaiXUSqiVUCuhVkKthFoJtRJqJdRKqJVQK6FWQq2EWgm1Emol1EqolVAroVZCrYRaCbUSaiXUSqiVUCuhVkKt",
    "hFoJtRJqJbOuwn6F_Qr7FfYrnFc4r3Be4bzCeYXzCucVziucVzivcF7hvMJ5hfMK5xXOK5xXOK9wXuG8wnmF8wrnFc4r",
    "8jvhGSc84wTJinRXpLsi3RWLZEW5a8pdU-6acteUu6bcNeWuKXdNuWvKXVPumnLX-K3xW-O3xm9NuWtM15iuMV1jusZ0",
    "TUI1CdWYrjFdY7rGdI3pGtM1pmtM15iuMV1jusZ0jekaRzWma0zXmK4pd025a8pdU-6acteUu6bcNeWuKXdNuWvKXVPu",
    "mnLXlLum3DXlril3TblrcNbwq-FXw6-GXw2_Bn4N_Br4NfBr4NfAr4FfA78Gfg38Gvg18Gvg18CvgV8Dv5bxWsZrGa9l",
    "vJbxWsZrGa9lvJbxWsZrGa9lvJbxWsZrGa89jkc9WurRUo-WerTUo6UeLfVoqUdLPVrq0VKPlnq01KOlHi31aKlHSz1a",
    "6tFSj5Z6tNSjpR4t9WipR0s9WurRUo-WerTUo6UeLfVoqUdLPVrq0VKPlnq01KOlHi31aKlHSz1a6tFSj5Z6tNSjpR4t",
    "9WipRwe_Dn4d_Dr4dfDr4NdBqANJB5IOJB0gOkB0gOgA0QGiw36H_Q77HfY7fHT46EiyI8mOJDuS7EiyI8mOJDuS7Ejy",
    "HGrnUDuH2jnUzqF2DrVzqJ1D7Rxq51A7h9o5TzuHkCNxR2kdpXWU1uHD4cPhw1FaR2kdjhyOHI4cjhyOHKV1lNbhzeHN",
    "4c3hzZGkI0lHko4kHUk6knQk6UjSkaQjSUeSjiQdSByldZTWUVp3TJzSOkrrKK2DmoOag5qDmoOag5qDmgOYo6qOqjqq",
    "2lPVnqr2VLWnqj1V7alqz4LoWRA9C6JnQfQsiB5qPdR6qPVQ66HWQ62HWg-1Hmo91Hqo9VDrodZDrYdaD7Ueaj3Ueqj1",
    "UOuh1kOth1oPtR5qPdR6qPVQ66HWQ62HWg-1Hmo91Hqo9VDrodZDrYdaD7Ueaj3Ueqj1UOuh1kOth1rPXOtB14OuB10P",
    "ugF0A-gG0A2gG0A3gG5gQxngN8BvgN8AvwF-A_wG-A3wG-A3wG-A3wC_AX4D_Ab4DfAb4DfAb4DfAL8BfgP8BvgN8Bvg",
    "N8BvgN8AvwF-A_wG-A3wG-A3wG-A3wC_AX4D_Ab4DfAb4DfAb4DfAL8BfgP8BvgN8BvgN8BvgN8AvwF-A_wG-A3wG-Dn",
    "4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh",
    "5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4efh5-Hn4bdmvDXjrRlvzXhr",
    "xlsz3prx1oy3Zrw1460Zb814a8YbKcBIAUYKMFKAkQKMFGCkACMFGCnASAFGCjBSgJECjBRgpAAjBRgpwEgBRgowUoCR",
    "AowUYKQAIwUYKcBIAUYKMFKAkQKMFGCkACMFGCnASAFGCjBSgJECjBRgpAAjBRgpwEgBRgowUoCRAowUYKQAIwUYATYC",
    "bATYCLARYCPARoCNABsBNgJsBNgIsPEIjAKMFGCkAAF-AX4BfgF-AX4BfgF-AXQBVgFWAVYBVgFWAVYBVgFWAVYBVgFW",
    "AVYBVgFWAVYBVgFWAVYBTAFMAUwBTAFMAUwBTAFMAUwBTAFMAUwBTAEuAS4BLgEuAS4BLgEuAS4BLgEuAS7hcfHX9eKv",
    "V4vpp-fF9PPnxfTuL7EdFtOHMrYqtpPYVrHVsTWxrWMbF9PyYjFlWWyXscXPZ9ex3cS2i-0htjhG9riY4hfL6STqT0Js",
    "3xfT6qfYnmKLz4s36ylerKd4r57itXqKt-op3pOneDme4u13ihfdKd5up3ivneK1dop32SneXad4V53iVXWKN9ApXkCn",
    "eP-c4i1zivfLqTmPLX6mifqmjy1-pom5NvGZzetiine8Kd7upni5m-LdbopXuyle6qZ4nZvibW6Kt7cpXs-meOua4qVr",
    "6j7FFn3G29DUFbFF_138t45_u43tPraYR7zTTKfx-adRfxo5ni5jS2KLnz2NzzyNzzyNzzuNnk5_iW0fW_z8WfR89jG2",
    "mO-ZW0zf4r_FK8AUbwDT5n1sH2L7uPjbT9vYXhZ_-7KKjWn0nmK_p9jvmcjvmcifmL6fjj9kM42WTJ4lk2fJ5FkyeZZM",
    "niWTZ8nkWTJ5lqyx5fFH3OPvlEzalEmb8qCU-ZoyX1NWdMpUzdg6MpZDxnLIWA4Zsz5j1mcMkDFAxqzPGCVjlIxRMmZ9",
    "xqzPGC87jsdUzZil2fFXM8yY4-8Sx6-ObFkVW1bFNK9wXvGJisld8bGK7Cs-WzG5Kyb3CXveCR87Id0T0j0h3RPW7Alc",
    "TqBxAsQTVszJ8esfEFfsASv2gBVre8XaXrG2V2S_IvsV2a_gvGKlrhhqRX4rUlsdr7c8t-ORHYQ6NoyOVd7xsY5V3vHZ",
    "jkJ1JNRRmQ6D3X7xp-ntr4s__fdDxus_HLv5zT8eu_nNPx27-c0_H7v5zb8cu_nNvx67-c2_Hbv5zb8fu_nNfxy7-c1_",
    "Hrv5zX8du_nN388_DrFdKFgrGBUEBRsFVwpuFOwU3Cr4ruBewYOCg4JHBc8KXhQwcT563ijwCtYKRgVBwUbBpYIrBdcK",
    "bhTsFOwV3Cr4ruBewYOCg4InBc8KXhUck79Q8hfCe6HkL5TzhXK-UM4Xyplgq-BGwU7BXsGtgjsF3xXcK3hU8KTgWcGL",
    "glcFx-TXwrtWhmsxXCuxtfJZK421Pj7K-yjvowYcNeAoy6NSHcV5FOdRyQdhCUojKI2gNILSCBpwozQ2mj8blWCjfDaq",
    "xUaJbZTYRg_dqAQbPWujEmxEfiPymz8-_UHBQcGjgmcFR_KXsnOpR1zqEZca8FLjXGqcK-G9kq8r-bpSzlca8EoDXgnU",
    "lXK-0iOulPOVnnWtAa81zrU-fi3vW2m2YrgVw60YbvXxrdLY_vHj9wqeFbwqOM6oG5XyRqW8kfcbPeJGDG804I3s3MjO",
    "jdARPCk4PmunR-w08k5UdzK402zZyelOTnd6-k7kd7K8k-Wd0O3kfacS7JTzTjnvlPNOOe-U806gCF4U_O7iSGwvO3vZ",
    "2cvFXi72crEXw72S3yvVvVLdK8O98tnrWbdyeqeH3ulZd3rWnZ51J2J3Inanh979cZy9gnsFDwoOCp4VHL1_19Pv9fR7",
    "ubjXx--V_L0-_qBPEXgFFwrWCkYFGwWXCq4UXCvYKrhRsFOwV3Cr4F7Bg4KDgicFLwpeFRzJH5T8QTkflPNB5A_K-aCc",
    "D0r1oFQPSvWgVA9K9aBUD0r1oAwPyvAgmAcl9qjEHvX0R438qJEfZflRvh718Sd9_Em-nlSCJw34JPJPGvBJxX2W92c9",
    "4kVOXzXOq1z80LN-KMMfs53s9n_-_LOP7S-x9bFdze9__N9_3Mf2a2y3_98_3se2mUU_-Dp0_A_4316f59eX-fV1fv3B",
    "V5HfdMfX5_n1ZX59nV9_8OdIv-mOr8_z68v8-jq_ovs6677Ouq-z7uus-zrrlrNuOeuWs24565azLpl1yaxLZl0y65L5",
    "jwrS9_NfHByDZwUvCl4VHMm90Z8nvNGfKLzRnym80Z8qvJH4rcRvJX4r8VuJ30r8TuJ3Er-T-J3E7yT-KPFHiT9K_FHi",
    "jxJ_kfiLxF8k_iLxF4m_SvxV4q8Sf5X4q8RLiZcSLyVeSrycxUbojNAZoTNCZ4TOCJ0ROiN0RuiM0JkPEn-Q-IPEHyT-",
    "ILHQGaEzQmeEzgid-STxJ4k_SfxJ4k8Sf5b4s8SfJf4s8WeJVRSjohgVxagoRkUxKopRUYyKYlQUo6IYFcWoKEZFMSqK",
    "-b0oicSJxInEicRaKSaVOJU4lTiVOJU4kziTOJM4kziTOJc4lziXOJc4l3iQeJB4kHiQeJDYS-wl9hJ7ib3EFxJfSHwh",
    "8YXEFxKvJV5LvJZ4LfFa4iBxkDhIHCQOEm8k3ki8kXgj8UbiS4kvJb6U-FLiS4mvJL6S-EriK4mvJL6W-Fria4mvJb6W",
    "eCvxVuKtxFuJtxLfSHwj8Y3ENxLfSLyTeCfxTuKdxDuJ9xLvJd5LvJd4L_GtxLcS30p8K_GtxHcS30l8J_GdxHcSf5f4",
    "u8TfJf4u8XeJ7yW-l_he4nuJ7yV-kPhB4geJHyR-kPhZ4meJnyV-lvh5FtufZ_ExeFbwouBVwVGszdxqM7fazK02c6vN",
    "3Gozt9rMrTZzq83cajO3Oo6tjmOr49jqOLY6jq12fqud32rnt9r5rXZ-q53faue32vmtdn6rnd9q-7Lavqy2L6vty2r7",
    "skZiI7GR2EhsJB4lHiUeJR4lHmdxpSlaaYpWmqKVpmilKVppilaaopWmaKUpWmmKVo8SP0r8KPGjxI-z-CSfxcfgWcGL",
    "glcFR3ElcSVxJXElcSXxicQnEp9IfCLxySxuNDcazY1Gc6PR3Gg0NxrNjUZzo9HcaDQ3Gs2NRnOj0dxoNDcazY1Gc6PR",
    "cdzoOG50HDc6jhsdx42O40bHcaPjuNFx3Og4bnQcNzqOGx3HjY7jRsdxo-O40XHc6DhudBw3Oo4bzedG87nRfG40nxvN",
    "5_bXWXwMnhW8KHhVcBRriraaoq2maKsp2mqKtpqiraZoqynaaoq2mqKtdtFWu2irXbTVLtpqF-1-msXH4FnBi4JXBUex",
    "itKpKJ2K0qkonYriJHYSO4mdxO53sTg7cXbi7MTZibOrJa4lriWuJa4lbiRuJG4kbiRuJG4lbiVuJW4lbiXuJO4k7iTu",
    "JO4kPpX4VOJTiU8lPpX4TOIzic8kPpP4TOJvEn-T-JvE3yT-JvG5xOcSn0t8LvG5xLoxOt0YnW6MTjdGpxuj20q8lXgr",
    "8VbircS6yTjdZJxuMk43GaebjNNNxukm43STcbrJON1knG4yTjcZp5uM003G6SbjtAad1qDTGnRag05r0GkNOq1BpzXo",
    "tAad1qDTGnRag05r0GkNOq1Bp5uM003G6SbjdJNxusk43WScbjJONxmnm4zTTcYdJD5IfJD4IPFBYl17nK49Ttcep2uP",
    "07XHvUj8IvGLxC8Sv0j8KvGrxK8Sv0r8Oot70ehFoxeNXjR60RhUlEFFGVSUQUUZVJRBRRlUlEFFGVSUQUUZxHkQ50Gc",
    "B3EexHkQ50GcB3EexHkQ5-FJ4ieJnyR-kvhpFnsdbV5Hm9fR5nW0eR1tXkeb19HmdbR5HW1eR5vXluu15XptuV5brteW",
    "63VV87qqeV3VvK5qXlc1byW2EluJrcRW4kLiQuJC4kLiQuJS4lLiUuJS4lJiXai8LlReFyqvC5XXhcrrQuV1ofK6UHld",
    "qLwuVH4l8UrilcQriVcS67TyOq28Tiuv08rrtPI6rbxOK6_Tyuu08jqtvE4rr9PK67TyOq28Tiuv08rrtPI6rbxOK6_T",
    "yuu08jqtvE4rr9PK67TyOq28Tiuv08rrtPI6rYK-1AR9qQn6UhP0pSboS03Qj3tBP-4F_bgX9ONe0I97QbfcoFtu0C03",
    "6JYbdMsNW4m3Em8l3kq8lVinVdBpFXRaBZ1WQadV0GkVdFoFnVZBp1XQaRV0WgWdVkGnVdBpFXRaBZ1WQadV0GkVdFoF",
    "nVZBG2PQxhi0MQZtjEEbY9DGGLQxBm2MQRtj0MYYtD8H7c9B-3PQ_hy0PwftokG7aNAuGrSLBu2iQbto0C4atIsG7aJB",
    "u2jQF7GgL2JBX8SCvoiF376I_S_AtPTI",
);

/// `EMOJI_COUNT` (emoji.cpp:113).
const EMOJI_COUNT: usize = 2370;

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
    // every zero-width joiner position are char boundaries. The search runs
    // over the C++ MAX_EMOJI_LENGTH + 4 window (emoji.cpp:142): an element
    // longer than the window cannot pass is_emoji_element, so a first
    // joiner beyond it is indistinguishable from none.
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

/// `skip_emoji_selector` (emoji.cpp:361-367) — one trailing variation
/// selector U+FE0F is part of the match.
fn skip_emoji_selector(str: &[u8], pos: usize) -> usize {
    if pos + 3 <= str.len() && str[pos] == 0xEF && str[pos + 1] == 0xB8 && str[pos + 2] == 0x8F {
        // skip \uFE0F
        return pos + 3;
    }
    pos
}

/// The longest emoji-table entry starting at the byte position (the
/// automaton's per-position length answer, emoji.cpp:283-300) — the
/// entries are at most MAX_EMOJI_LENGTH bytes, so the scan tries the
/// lengths high-to-low. The C++ reads the same answer out of the
/// Aho-Corasick `process` circular buffer; the linear scan over the same
/// table answers the identical longest match.
fn longest_emoji_at(table: &HashSet<&'static [u8]>, str: &[u8], position: usize) -> usize {
    let max_end = (position + MAX_EMOJI_LENGTH).min(str.len());
    for end in (position + 1..=max_end).rev() {
        if table.contains(&str[position..end]) {
            return end - position;
        }
    }
    0
}

/// `find_emojis` (emoji.h:16, emoji.cpp:369-413) — all non-overlapping
/// emoji in the string in order of their appearance; the longest emoji is
/// chosen at each position; every returned slice satisfies [`is_emoji`].
/// The C++ drives the per-position lengths out of the Aho-Corasick
/// automaton in 2048-byte batches; the collapsed scan reads the same
/// lengths off the emoji table directly (the observable contract is the
/// longest table match per position, one swallowed U+FE0F, then the
/// zero-width-joiner chain).
pub fn find_emojis(str: &str) -> Vec<&str> {
    let table = emoji_table();
    let bytes = str.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let length = longest_emoji_at(table, bytes, i);
        if length == 0 {
            // skip positions without emoji; the C++ skips ahead over the
            // already-known zero lengths, the scan just steps one byte
            // (a match start is always a UTF-8 lead byte, so table hits
            // realign the scan)
            i += 1;
            continue;
        }
        let mut end = skip_emoji_selector(bytes, i + length);
        while end + 3 < bytes.len()
            && bytes[end] == 0xE2
            && bytes[end + 1] == 0x80
            && bytes[end + 2] == 0x8D
        {
            // zero-width joiner \u200D
            let next_length = longest_emoji_at(table, bytes, end + 3);
            if next_length == 0 {
                break;
            }
            end = skip_emoji_selector(bytes, end + 3 + next_length);
        }
        // Every hop (a table entry, a 3-byte selector, a 3-byte joiner)
        // is a run of complete characters, so i..end are char boundaries.
        result.push(&str[i..end]);
        i = end;
    }
    result
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

    #[test]
    fn find_emojis_matches_the_cpp_contract() {
        // No emoji — the empty and plain-text bands.
        assert!(find_emojis("").is_empty());
        assert!(find_emojis("plain text, digits 123, no emoji").is_empty());
        // A selector alone is not an emoji (emoji.cpp:375-381).
        assert!(find_emojis("\u{FE0F}").is_empty());

        // Single entries.
        assert_eq!(find_emojis("⌚"), vec!["⌚"]);
        assert_eq!(find_emojis("👍"), vec!["👍"]);
        // Adjacent emoji are separate matches.
        assert_eq!(find_emojis("⌚⌛"), vec!["⌚", "⌛"]);
        // A regional-pair flag is one 8-byte entry.
        assert_eq!(find_emojis("🇺🇸"), vec!["🇺🇸"]);
        // The keycap is one entry over the digit + selector + combining key.
        assert_eq!(find_emojis("1️⃣"), vec!["1️⃣"]);
        // In running text the digit stays text unless the keycap follows:
        // in "12️⃣" the keycap entry is "2️⃣" and the leading '1' stays text.
        assert_eq!(find_emojis("abc12️⃣3"), vec!["2️⃣"]);

        // The longest entry wins at each position: thumb + skin tone is its
        // own table entry, so the pair is one match, not two.
        assert_eq!(find_emojis("👍🏻"), vec!["👍🏻"]);

        // A bare heart swallows one trailing variation selector...
        assert_eq!(find_emojis("❤"), vec!["❤"]);
        assert_eq!(find_emojis("❤\u{FE0F}"), vec!["❤\u{FE0F}"]);
        // ...but only one (emoji.cpp:386-388 — the second selector is left
        // unmatched and dropped).
        assert_eq!(find_emojis("❤\u{FE0F}\u{FE0F}"), vec!["❤\u{FE0F}"]);

        // Zero-width-joiner chains are one match while every joined element
        // is an emoji (emoji.cpp:389-394).
        assert_eq!(
            find_emojis("👨\u{200D}👩\u{200D}👦"),
            vec!["👨\u{200D}👩\u{200D}👦"]
        );
        // Chains compose over distinct entries.
        assert_eq!(find_emojis("👍\u{200D}⌚"), vec!["👍\u{200D}⌚"]);
        // A joiner without a following emoji breaks the chain: the match is
        // the first element, the dangling joiner is dropped (the
        // `end + 3 < str.size()` guard, emoji.cpp:391).
        assert_eq!(find_emojis("👍\u{200D}"), vec!["👍"]);

        // Mixed text.
        assert_eq!(find_emojis("привет 👍 мир"), vec!["👍"]);
        assert_eq!(find_emojis("a⌚b👍🏻c"), vec!["⌚", "👍🏻"]);

        // Every returned slice satisfies is_emoji (the emoji.h contract).
        for emoji in find_emojis("❤\u{FE0F}👍🏻x👨\u{200D}👩\u{200D}👦🇺🇸") {
            assert!(is_emoji(emoji), "{emoji:?} must satisfy is_emoji");
        }

        // An input longer than the C++ 2048-byte automaton buffer — the
        // batching is invisible in the answer.
        let long = format!("{}👍{}", "word ".repeat(500), " tail");
        assert_eq!(find_emojis(&long), vec!["👍"]);
    }
}
