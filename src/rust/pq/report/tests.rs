use super::*;
use crate::pq::cluster::Member;

    fn opts() -> ReportOpts {
        ReportOpts { metric: Metric::Cpu, top: 15, verbose: false, colors: false, width: None,
            compact_json: false, sample_interval_ms: 400 }
    }

    #[test]
    fn metric_value_scales_cpu_and_passes_through_memory_and_swap() {
        let c = Cluster { identity: "a".into(), cpu: 12.34, rss: 5_000, swap: 900, members: vec![] };
        assert_eq!(metric_value(&c, Metric::Cpu), 1234);
        assert_eq!(metric_value(&c, Metric::Memory), 5_000);
        assert_eq!(metric_value(&c, Metric::Swap), 900);
    }

    #[test]
    fn metric_capacity_and_used_scale_cpu_and_pass_through_memory_and_swap() {
        assert_eq!(metric_capacity(999, 500, Metric::Cpu), 10_000);
        assert_eq!(metric_capacity(999, 500, Metric::Memory), 999);
        assert_eq!(metric_capacity(999, 500, Metric::Swap), 500);
        assert_eq!(metric_used(12.34, 999, 500, Metric::Cpu), 1234);
        assert_eq!(metric_used(12.34, 999, 500, Metric::Memory), 999);
        assert_eq!(metric_used(12.34, 999, 500, Metric::Swap), 500);
    }

    #[test]
    fn top_clusters_caps_at_top_and_at_the_palette_size() {
        let clusters: Vec<Cluster> = (0..20)
            .map(|i| Cluster { identity: format!("c{i}"), cpu: 0.0, rss: 0, swap: 0, members: vec![] })
            .collect();
        // opts.top (15) exceeds the palette (12): capped at the palette size.
        let top = top_clusters(&clusters, 15);
        assert_eq!(top.len(), style::PALETTE.len());
        assert_eq!(top[0].identity, "c0");
        // A smaller opts.top caps it further.
        assert_eq!(top_clusters(&clusters, 3).len(), 3);
    }

    #[test]
    fn colored_by_assigns_palette_colors_by_rank() {
        let clusters = vec![
            Cluster { identity: "a".into(), cpu: 0.0, rss: 10, swap: 1, members: vec![] },
            Cluster { identity: "b".into(), cpu: 0.0, rss: 20, swap: 2, members: vec![] },
        ];
        let top = top_clusters(&clusters, 15);
        let by_rss = colored_by(&top, |c| c.rss);
        assert_eq!(by_rss, vec![
            ("a".to_string(), 10, style::PALETTE[0]),
            ("b".to_string(), 20, style::PALETTE[1]),
        ]);
        // Same identities/colors, different values, when valued by swap instead.
        let by_swap = colored_by(&top, |c| c.swap);
        assert_eq!(by_swap, vec![
            ("a".to_string(), 1, style::PALETTE[0]),
            ("b".to_string(), 2, style::PALETTE[1]),
        ]);
    }

    #[test]
    fn ring_segments_sum_to_capacity() {
        let colored = vec![
            ("a".to_string(), 40u64, style::PALETTE[0]),
            ("b".to_string(), 20u64, style::PALETTE[1]),
        ];
        let segs = ring_segments(&colored, 70, 100);
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[2].label, "other");
        assert_eq!(segs[2].value, 10); // used(70) - colored_sum(60)
        assert_eq!(segs[3].label, "unused");
        assert_eq!(segs[3].value, 30); // capacity(100) - used(70)
        let sum: u64 = segs.iter().map(|s| s.value).sum();
        assert_eq!(sum, 100);
    }

    #[test]
    fn ring_segments_saturate_when_colored_sum_exceeds_used() {
        // Colored clusters sum to 90 but the reported system "used" is only 50 (e.g. independent
        // rounding skew): "other" saturates to 0 rather than underflowing, and "unused" shrinks to
        // capacity minus the colored sum instead of capacity minus used.
        let colored = vec![("a".to_string(), 90u64, style::PALETTE[0])];
        let segs = ring_segments(&colored, 50, 100);
        assert_eq!(segs[1].label, "other");
        assert_eq!(segs[1].value, 0);
        assert_eq!(segs[2].label, "unused");
        assert_eq!(segs[2].value, 10); // 100 - max(90, 50)
        let sum: u64 = segs.iter().map(|s| s.value).sum();
        assert_eq!(sum, 100);
    }

    #[test]
    fn ring_segments_are_empty_parts_when_fully_accounted_for() {
        // Colored clusters already sum to the full "used" and "used" equals capacity: nothing left
        // over for either remainder, but both segments are still present (value 0).
        let colored = vec![("a".to_string(), 100u64, style::PALETTE[0])];
        let segs = ring_segments(&colored, 100, 100);
        assert_eq!(segs[1].value, 0);
        assert_eq!(segs[2].value, 0);
    }

    #[test]
    fn text_report_shows_header_and_cluster() {
        let clusters = vec![Cluster { identity: "gradle".into(), cpu: 30.0, rss: 3_000_000, swap: 0,
            members: vec![Member { pid: 100, cpu: 10.0, rss: 1_000_000, swap: 0, cmd: "java ...".into(), comm: "java".into() }] }];
        let mut buf = Vec::new();
        write_text(&mut buf, &clusters, 14_000_000, 31_000_000, 0, 0, 30.0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("gradle"));
        assert!(s.contains("cpu")); // header mentions cpu total
        assert!(!s.contains('\u{1b}')); // colors:false -> no ANSI
        assert!(!s.contains("swap")); // no swap configured -> header omits it entirely
    }

    #[test]
    fn text_report_header_shows_swap_when_system_has_it() {
        let clusters = vec![Cluster { identity: "gradle".into(), cpu: 30.0, rss: 3_000_000, swap: 0, members: vec![] }];
        let mut buf = Vec::new();
        // System has swap configured (swap_total>0) but none in use: header shows it, table doesn't.
        write_text(&mut buf, &clusters, 14_000_000, 31_000_000, 0, 8_000_000_000, 30.0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("swap"));
    }

    #[test]
    fn text_report_swap_column_appears_when_swap_in_use() {
        let clusters = vec![Cluster { identity: "gradle".into(), cpu: 30.0, rss: 3_000_000, swap: 2_000_000, members: vec![] }];
        let mut buf = Vec::new();
        write_text(&mut buf, &clusters, 14_000_000, 31_000_000, 2_000_000, 8_000_000_000, 30.0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // human_plain(2_000_000) == "2M": once in the header's swap total, once in the row's swap
        // column, since both the system swap_used and this cluster's swap happen to be 2_000_000.
        assert_eq!(s.matches("2M").count(), 2);
    }

    #[test]
    fn verbose_members_render_with_and_without_swap_and_width_limits() {
        let cluster = Cluster {
            identity: "a-very-long-cluster-identity".into(), cpu: 30.0, rss: 3_000_000, swap: 2_000_000,
            members: vec![
                Member { pid: 100, cpu: 20.0, rss: 2_000_000, swap: 1_500_000,
                    cmd: "a very long worker command line".into(), comm: "worker".into() },
                Member { pid: 101, cpu: 10.0, rss: 1_000_000, swap: 500_000,
                    cmd: "another long worker command line".into(), comm: "worker".into() },
            ],
        };
        let mut options = opts();
        options.verbose = true;
        options.width = Some(72);
        for swap_used in [0, 2_000_000] {
            let mut output = Vec::new();
            write_text(&mut output, std::slice::from_ref(&cluster), 3_000_000, 10_000_000,
                swap_used, 8_000_000, 30.0, &options).unwrap();
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains("pid 100"));
            assert!(text.contains("2 procs"));
        }
    }

    #[test]
    fn json_report_is_wellformed() {
        let clusters = vec![Cluster { identity: "gradle".into(), cpu: 30.0, rss: 3_000_000, swap: 0, members: vec![] }];
        let mut buf = Vec::new();
        write_json(&mut buf, &clusters, 14_000_000, 31_000_000, 0, 0, 30.0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"clusters\""));
        assert!(s.contains("\"identity\": \"gradle\""));
        assert!(s.contains("\"swap_used\""));
    }

    #[test]
    fn json_report_includes_swap_for_a_swapping_cluster() {
        let clusters = vec![Cluster { identity: "gradle".into(), cpu: 30.0, rss: 3_000_000, swap: 2_000_000,
            members: vec![Member { pid: 100, cpu: 30.0, rss: 3_000_000, swap: 2_000_000, cmd: "java ...".into(), comm: "java".into() }] }];
        let mut buf = Vec::new();
        write_json(&mut buf, &clusters, 14_000_000, 31_000_000, 2_000_000, 8_000_000_000, 30.0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"swap_used\": 2000000"));
        assert!(s.contains("\"swap\": 2000000"));
    }

    #[test]
    fn compact_json_is_one_line_and_has_sample_metadata() {
        let clusters = vec![Cluster { identity: "one".into(), cpu: 1.0, rss: 2, swap: 0, members: vec![] }];
        let mut o = opts();
        o.compact_json = true;
        o.sample_interval_ms = 750;
        let mut buf = Vec::new();
        write_json(&mut buf, &clusters, 0, 1, 0, 0, 1.0, &o).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 1);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["sample_interval_ms"], 750);
        assert!(value["generated_at_unix_ms"].as_u64().is_some());
    }

    #[test]
    fn cpu_ring_helpers_attribute_known_processes_and_fill_idle_time() {
        let clusters = vec![Cluster {
            identity: "alpha".into(), cpu: 30.0, rss: 10, swap: 0,
            members: vec![Member { pid: 7, cpu: 30.0, rss: 10, swap: 0, cmd: "alpha".into(), comm: "alpha".into() }],
        }];
        let colors = pid_colors(&clusters, 1);
        assert_eq!(colors.get(&7), Some(&style::PALETTE[0]));
        let slices = core_ring_slices(&[(7, 25.0), (99, 5.0)], 40.0, &colors);
        assert!(slices.contains(&(2500, style::PALETTE[0])));
        assert!(slices.contains(&(1500, style::OTHER_COLOR)));
        assert!(slices.contains(&(6000, UNUSED_COLOR)));

        let attributed = build_cpu_rings(&[40.0], &[vec![(7, 25.0)]], &clusters, 1);
        assert_eq!(attributed.len(), 1);
        let fallback = build_cpu_rings(&[-5.0, 120.0], &[], &clusters, 1);
        assert_eq!(fallback[0].iter().map(|slice| slice.0).sum::<u64>(), 10_000);
        assert_eq!(fallback[1].iter().map(|slice| slice.0).sum::<u64>(), 10_000);
    }

    #[test]
    fn legend_renderers_cover_clusters_other_and_unused_rows() {
        let clusters = vec![
            Cluster { identity: "alpha".into(), cpu: 20.0, rss: 200, swap: 20, members: vec![] },
            Cluster { identity: "beta-with-a-long-name".into(), cpu: 10.0, rss: 100, swap: 10, members: vec![] },
        ];
        let top = top_clusters(&clusters, 1);
        let colored = colored_by(&top, |cluster| metric_value(cluster, Metric::Memory));
        let segments = ring_segments(&colored, 400, 1000);
        let mut options = opts();
        options.metric = Metric::Memory;
        options.width = Some(36);
        let mut output = Vec::new();
        write_legend(&mut output, &segments, &clusters, &options).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("alpha"));
        assert!(text.contains("other"));
        assert!(text.contains("unused"));

        let outer = ring_segments(&colored_by(&top, |cluster| cluster.rss), 400, 1000);
        let inner = ring_segments(&colored_by(&top, |cluster| cluster.swap), 40, 100);
        let mut output = Vec::new();
        write_legend_two_tier(&mut output, &top, &clusters, &outer, &inner, &options).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("swap"));
        assert!(text.contains("other"));
        assert!(text.contains("unused"));
    }

    #[test]
    fn graphics_falls_back_to_text_when_capacity_is_zero() {
        let clusters = vec![Cluster { identity: "zero".into(), cpu: 0.0, rss: 0, swap: 0, members: vec![] }];
        let mut options = opts();
        options.metric = Metric::Memory;
        let mut output = Vec::new();
        write_graphics(&mut output, &clusters, 0, 0, 0, 0, 0.0, &[], &[], &options).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("zero"));
    }

    #[test]
    fn injectable_graphics_covers_cpu_single_and_two_tier_success_and_failure() {
        let clusters = vec![Cluster {
            identity: "alpha".into(), cpu: 40.0, rss: 400, swap: 40,
            members: vec![Member { pid: 7, cpu: 40.0, rss: 400, swap: 40, cmd: "alpha".into(), comm: "alpha".into() }],
        }];
        let mut options = opts();
        options.width = Some(32);

        let mut cpu = Vec::new();
        write_graphics_with(&mut cpu, &clusters, 500, 1000, 0, 0, 40.0, &[40.0],
            &[vec![(7, 40.0)]], &options, |image, config| {
                assert!(image.width() > 0);
                assert!(config.width.is_some());
                true
            }).unwrap();
        assert!(String::from_utf8(cpu).unwrap().contains("alpha"));

        options.metric = Metric::Memory;
        for succeeds in [true, false] {
            let mut two_tier = Vec::new();
            write_graphics_with(&mut two_tier, &clusters, 500, 1000, 50, 100, 40.0, &[], &[],
                &options, |_, _| succeeds).unwrap();
            assert!(String::from_utf8(two_tier).unwrap().contains("alpha"));
        }

        options.metric = Metric::Swap;
        let mut single = Vec::new();
        write_graphics_with(&mut single, &clusters, 500, 1000, 40, 0, 40.0, &[], &[],
            &options, |_, _| false).unwrap();
        assert!(String::from_utf8(single).unwrap().contains("alpha"));
    }
