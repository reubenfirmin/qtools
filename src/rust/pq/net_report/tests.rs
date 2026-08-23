    use std::collections::{HashMap, HashSet};

    use super::*;
    use crate::pq::net::{NetCluster, NetReport, Proto, Sock, SockState};

    fn sock(port: u16, state: SockState) -> Sock {
        Sock {
            proto: Proto::Tcp, state,
            local: "127.0.0.1".parse().unwrap(), local_port: port,
            peer: "0.0.0.0".parse().unwrap(), peer_port: 0,
            uid: 1000, inode: 1
        }
    }

    /// A bound-but-unconnected UDP socket on the wildcard address: state Other(7) with no peer,
    /// which is_listening() treats as listening, state_label() renders as "BOUND", and
    /// is_world_exposed() flags (0.0.0.0 bind).
    fn udp_bound(port: u16) -> Sock {
        Sock {
            proto: Proto::Udp, state: SockState::Other(7),
            local: "0.0.0.0".parse().unwrap(), local_port: port,
            peer: "0.0.0.0".parse().unwrap(), peer_port: 0,
            uid: 1000, inode: 2
        }
    }

    /// An outbound established connection (ephemeral local port, real peer).
    fn estab(local_port: u16, peer_port: u16) -> Sock {
        Sock {
            proto: Proto::Tcp, state: SockState::Established,
            local: "192.168.1.5".parse().unwrap(), local_port,
            peer: "34.107.243.93".parse().unwrap(), peer_port,
            uid: 1000, inode: 3
        }
    }

    fn report() -> NetReport {
        NetReport {
            clusters: vec![NetCluster {
                identity: "nginx".into(), cpu: 2.0, rss: 50_000_000, proc_count: 4,
                conns: vec![
                    (sock(80, SockState::Listen), 500),
                    (sock(443, SockState::Listen), 500),
                    (udp_bound(53), 500),
                    (estab(44000, 443), 500)
                ],
                listen_ports: vec![53, 80, 443],
                world_ports: vec![53]
            }],
            unattributed: vec![(989, vec![sock(5432, SockState::Listen)])],
            total_conns: 5,
            total_listening: 4,
            unattributed_count: 1,
            listen_port_set: [53u16, 80, 443, 5432].into_iter().collect(),
            total_world: 1,
            // The established conn (inode 3) moved 1.5M out, 300K in.
            traffic: [(3u64, (1_500_000u64, 300_000u64))].into_iter().collect()
        }
    }

    fn opts() -> NetOpts {
        NetOpts { top: 15, verbose: false, colors: false, width: None, port: None,
            compact_json: false, sample_interval_ms: 400, names: DisplayNames::default() }
    }

    #[test]
    fn grid_shows_totals_ports_and_hints() {
        let mut buf = Vec::new();
        write_text(&mut buf, &report(), 14_000_000, 31_000_000, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("5 conns"));
        assert!(s.contains("4 listening"));
        assert!(s.contains("1 world")); // header world count (plain text when colors off)
        assert!(s.contains("listening on")); // grid column headings
        assert!(s.contains("talking to"));
        assert!(s.contains(&human_plain(1_500_000))); // tx bytes in the grid
        assert!(s.contains(&human_plain(300_000))); // rx bytes in the grid
        assert!(s.contains("nginx (4)"));
        assert!(s.contains(":53 :80 :443")); // listening cell
        assert!(s.contains(":443")); // talking cell (outbound peer port)
        assert!(!s.contains("cpu")); // no cpu/mem in net mode
        assert!(s.contains("(unattributed)"));
        assert!(s.contains(":5432")); // unattributed rows show their ports too
        assert!(s.contains("run with sudo"));
        assert!(s.contains("pq --net --listen (every listener)")); // usage footer
        assert!(!s.contains('\u{1b}')); // colors:false -> no ANSI
        assert!(!s.contains("127.0.0.1:80")); // per-connection rows only with -v
    }

    #[test]
    fn world_count_absent_when_zero() {
        let mut r = report();
        r.total_world = 0;
        let mut buf = Vec::new();
        write_text(&mut buf, &r, 0, 0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains("world"));
    }

    #[test]
    fn verbose_grid_lists_connections_with_direction() {
        let mut buf = Vec::new();
        let o = NetOpts { verbose: true, ..opts() };
        write_text(&mut buf, &report(), 0, 0, &o).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("listen  pid 500  tcp LISTEN  127.0.0.1:80"));
        assert!(s.contains("out     pid 500  tcp ESTAB  192.168.1.5:44000 -> 34.107.243.93:443"));
        assert!(s.contains("[world]")); // the wildcard-bound UDP listener
    }

    #[test]
    fn no_hint_when_everything_attributed() {
        let mut r = report();
        r.unattributed.clear();
        r.unattributed_count = 0;
        let mut buf = Vec::new();
        write_text(&mut buf, &r, 0, 0, &opts()).unwrap();
        assert!(!String::from_utf8(buf).unwrap().contains("run with sudo"));
    }

    #[test]
    fn listen_cell_overflows_gracefully() {
        let ports: Vec<u16> = (1..=10).map(|i| i * 1000).collect();
        let cell = listen_cell(&ports, 26);
        assert!(cell.len() <= 26, "cell must fit its column: {:?} ({})", cell, cell.len());
        assert!(cell.contains("(+"), "overflow must be marked: {cell}");
        assert!(listen_cell(&[80, 443], 26).eq(":80 :443")); // short lists render whole
        assert!(listen_cell(&[], 26).is_empty());
    }

    #[test]
    fn detail_answers_port_query_with_cards() {
        let procs = vec![proc::Proc {
            pid: 500, ppid: 1, comm: "nginx".into(),
            cmdline: vec!["/usr/sbin/nginx".into(), "-g".into(), "daemon off;".into()],
            rss: 50_000_000, swap: 0, uid: 0, cpu_pct: 2.0
        }];
        let o = NetOpts { port: Some(80), ..opts() };
        let mut buf = Vec::new();
        write_net_detail(&mut buf, &report(), &procs, 0, 0, &o).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("port 80 is held by nginx")); // the plain-English answer
        assert!(s.contains("open to the WORLD"));
        assert!(s.contains('\u{250c}') && s.contains('\u{2514}')); // a real box
        assert!(s.contains("pid 500  user root  2% cpu"));
        assert!(s.contains("/usr/sbin/nginx -g daemon off;")); // full command line
        assert!(s.contains("WORLD \u{2550}\u{2550}\u{25b6} udp 0.0.0.0:53"));
        assert!(s.contains("local \u{2500}\u{2500}\u{25b6} tcp 127.0.0.1:80"));
        assert!(s.contains("this machine only")); // plain-language reachability note
        assert!(s.contains("out   \u{2500}\u{2500}\u{25b6} tcp 34.107.243.93:443")); // peer, not noise
        assert!(s.contains(&format!("{} tx / {} rx",
            human_plain(1_500_000), human_plain(300_000)))); // traffic on the established row
        assert!(s.contains("free the port: pq --port 80 --kill")); // actionable hint
        assert!(s.contains("(unattributed)"));
        assert!(s.contains("run with sudo"));
    }

    #[test]
    fn detail_caps_pid_blocks() {
        let mut r = report();
        // 6 pids, one socket each: only the first 4 get blocks, the rest are summarized.
        r.clusters[0].conns = (0..6).map(|i| (sock(8000 + i as u16, SockState::Listen), 100 + i)).collect();
        let mut buf = Vec::new();
        write_net_detail(&mut buf, &r, &[], 0, 0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("+ 2 more procs holding 2 socket(s)"));
    }

    #[test]
    fn detail_reports_empty_port() {
        let r = NetReport {
            clusters: vec![], unattributed: vec![], total_conns: 0, total_listening: 0,
            unattributed_count: 0, listen_port_set: HashSet::new(), total_world: 0,
            traffic: HashMap::new()
        };
        let o = NetOpts { port: Some(9999), ..opts() };
        let mut buf = Vec::new();
        write_net_detail(&mut buf, &r, &[], 0, 0, &o).unwrap();
        assert!(String::from_utf8(buf).unwrap().contains("nothing on this host has local port 9999"));
    }

    #[test]
    fn detail_empty_pattern_and_exposure_sentences_cover_non_world_cases() {
        let empty = NetReport { clusters: Vec::new(), unattributed: Vec::new(), total_conns: 0,
            total_listening: 0, unattributed_count: 0, listen_port_set: HashSet::new(),
            total_world: 0, traffic: HashMap::new() };
        let mut output = Vec::new();
        write_net_detail(&mut output, &empty, &[], 0, 0, &opts()).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("nothing matches"));

        let mut local = report();
        local.clusters[0].conns.retain(|(socket, _)| socket.local_port == 80);
        local.clusters[0].listen_ports = vec![80];
        local.clusters[0].world_ports.clear();
        local.unattributed.clear();
        local.unattributed_count = 0;
        local.total_conns = 1;
        local.total_listening = 1;
        let sentence = port_sentence(&local, 80, false);
        assert!(sentence.contains("local/specific"));

        let mut established = report();
        established.clusters[0].conns.retain(|(socket, _)| !socket.is_listening());
        established.clusters[0].listen_ports.clear();
        established.clusters[0].world_ports.clear();
        established.unattributed.clear();
        established.total_conns = 1;
        established.total_listening = 0;
        assert!(port_sentence(&established, 443, false).contains("only established traffic"));

        let mut interface = sock(8080, SockState::Listen);
        interface.local = "192.168.1.10".parse().unwrap();
        assert_eq!(listener_scope(&interface).0, "iface");
    }

    #[test]
    fn json_report_is_wellformed_and_always_includes_connections() {
        let mut buf = Vec::new();
        write_json(&mut buf, &report(), 14_000_000, 31_000_000, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let value: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(s.contains("\"mode\": \"net\""));
        assert!(s.contains("\"total_conns\": 5"));
        assert!(s.contains("\"total_world\": 1"));
        assert!(s.contains("\"identity\": \"nginx\""));
        assert!(s.contains("\"listeners\": 3"));
        assert!(s.contains("\"established\": 1"));
        assert_eq!(value["clusters"][0]["listening"], serde_json::json!([53, 80, 443]));
        assert_eq!(value["clusters"][0]["world_ports"], serde_json::json!([53]));
        assert!(s.contains("\"bytes_tx\": 1500000")); // cluster traffic sums
        assert!(s.contains("\"bytes_rx\": 300000"));
        assert!(s.contains("\"tx\": 1500000")); // per-connection counters
        assert!(s.contains("\"local\": \"127.0.0.1:80\"")); // no -v gating in JSON
        assert!(s.contains("\"listening\": true")); // per-connection listening field
        assert!(s.contains("\"state\": \"BOUND\"")); // UDP bound socket, consistent with text
        assert!(s.contains("\"direction\": \"listen\""));
        assert!(s.contains("\"direction\": \"out\"")); // the established conn
        assert!(s.contains("\"world\": true")); // the wildcard-bound listener
        assert!(s.contains("\"unattributed\""));
        assert!(s.contains("\"uid\": 989"));
    }

    #[test]
    fn cutoff_row_names_hidden_clusters() {
        let mut r = report();
        r.clusters.push(NetCluster {
            identity: "extra".into(), cpu: 0.0, rss: 0, proc_count: 1,
            conns: vec![(sock(9000, SockState::Listen), 1)],
            listen_ports: vec![9000], world_ports: vec![]
        });
        let o = NetOpts { top: 1, ..opts() };
        let mut buf = Vec::new();
        write_text(&mut buf, &r, 0, 0, &o).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // The hidden cluster is LISTENING, so the cutoff row must name its port.
        assert!(s.contains("+ 1 more cluster(s), 1 conns, still LISTENING on :9000"));
        assert!(!s.contains("extra")); // the cluster itself stays hidden

        // Hidden clusters with no listeners get the plain cutoff wording.
        let mut r2 = report();
        r2.clusters.push(NetCluster {
            identity: "chatty".into(), cpu: 0.0, rss: 0, proc_count: 1,
            conns: vec![(estab(50000, 443), 1)],
            listen_ports: vec![], world_ports: vec![]
        });
        let mut buf2 = Vec::new();
        write_text(&mut buf2, &r2, 0, 0, &o).unwrap();
        let s2 = String::from_utf8(buf2).unwrap();
        assert!(s2.contains("(raise -n to see them)"));
        assert!(!s2.contains("still LISTENING"));
    }

    #[test]
    fn swatches_mark_rows_in_graphics_body() {
        let mut buf = Vec::new();
        write_body(&mut buf, &report(), &opts(), true).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // colors:false renders swatches as plain blocks: one for nginx, one for unattributed.
        assert_eq!(s.matches("\u{2588}\u{2588}").count(), 2);
        // Text mode has none.
        let mut buf2 = Vec::new();
        write_body(&mut buf2, &report(), &opts(), false).unwrap();
        assert!(!String::from_utf8(buf2).unwrap().contains('\u{2588}'));
    }

    #[test]
    fn outbound_summary_groups_and_caps_peer_ports() {
        let listen: HashSet<u16> = [80u16].into_iter().collect();
        let mut c = report().clusters.remove(0);
        // Add more outbound conns: two more to 443, one each to 4 distinct ports.
        for (i, peer) in [443u16, 443, 993, 465, 587, 22].iter().enumerate() {
            c.conns.push((estab(50000 + i as u16, *peer), 500));
        }
        let s = outbound_summary(c.conns.iter().map(|(s, _)| s), &listen, &DisplayNames::default());
        assert!(s.starts_with(":443 x3"), "443 should lead with its count: {s}");
        assert!(s.contains("(+2)"), "overflow beyond top 3 is counted: {s}");
        // Listeners alone produce no summary.
        assert!(outbound_summary([sock(80, SockState::Listen)].iter(), &listen, &DisplayNames::default()).is_empty());
    }

    #[test]
    fn service_names_annotate_outbound_ports() {
        let c = &report().clusters[0];
        let listen: HashSet<u16> = [53, 80, 443, 5432].into_iter().collect();
        let names = DisplayNames { services: [(443, "https".to_string())].into_iter().collect(), hosts: HashMap::new() };
        assert!(outbound_summary(c.conns.iter().map(|(s, _)| s), &listen, &names).contains(":443(https)"));
    }

    #[test]
    fn paint_port_respects_digit_boundaries() {
        // ":53" must not recolor the front of ":5432".
        let painted = paint_port("x  :53 :5432", 53, WORLD_COLOR);
        assert!(painted.contains(":5432"));
        assert_eq!(painted.matches('\u{1b}').count(), 2); // one color-on, one reset
    }

    #[test]
    fn conn_segments_sum_to_total_conns() {
        let r = report(); // nginx has 4 conns, 1 unattributed, total 5
        let segs = conn_segments(&r, 15, |c| c.conns.len() as u64, r.total_conns as u64);
        assert_eq!(segs.len(), 2); // nginx + other
        assert_eq!(segs[0].label, "nginx");
        assert_eq!(segs[0].value, 4);
        assert_eq!(segs[1].label, "other");
        assert_eq!(segs[1].value, 1); // the unattributed socket
        let sum: u64 = segs.iter().map(|s| s.value).sum();
        assert_eq!(sum, r.total_conns as u64);
    }

    #[test]
    fn two_ring_segments_split_established_and_listeners() {
        let r = report();
        let est = conn_segments(&r, 15, |c| c.established() as u64, (r.total_conns - r.total_listening) as u64);
        assert_eq!(est[0].value, 1); // nginx's outbound conn
        assert_eq!(est[1].value, 0); // nothing else established
        let lsn = conn_segments(&r, 15, |c| c.listeners() as u64, r.total_listening as u64);
        assert_eq!(lsn[0].value, 3); // nginx's three listeners
        assert_eq!(lsn[1].value, 1); // the unattributed postgres listener
    }

    #[test]
    fn injectable_graphics_covers_two_ring_single_ring_empty_and_failure_paths() {
        for succeeds in [true, false] {
            let mut output = Vec::new();
            write_graphics_with(&mut output, &report(), 10, 100, &opts(), |image, config| {
                assert!(image.width() > 0);
                assert!(config.width.is_some());
                succeeds
            }).unwrap();
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains("nginx"));
            assert_eq!(text.contains("outer ring:"), succeeds);
        }

        let mut single = report();
        single.total_listening = single.total_conns;
        let mut output = Vec::new();
        write_graphics_with(&mut output, &single, 0, 0, &opts(), |_, _| true).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("nginx"));

        let empty = NetReport { clusters: Vec::new(), unattributed: Vec::new(), total_conns: 0,
            total_listening: 0, unattributed_count: 0, listen_port_set: HashSet::new(),
            total_world: 0, traffic: HashMap::new() };
        let mut output = Vec::new();
        write_graphics_with(&mut output, &empty, 0, 0, &opts(), |_, _| panic!("empty report drew an image")).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("talking to"));
    }

    #[test]
    fn render_dispatches_machine_text_and_graphics_detail_targets() {
        let report = report();
        let machine_options = NetOpts { compact_json: true, ..opts() };
        let mut output = Vec::new();
        render(&mut output, &report, &[], 0, 0, &machine_options,
            RenderContext { target: RenderTarget::JsonLines, colors: false, width: None }, false).unwrap();
        assert_eq!(String::from_utf8_lossy(&output).lines().count(), 1);

        for (target, detail) in [
            (RenderTarget::Text, false),
            (RenderTarget::Text, true),
            (RenderTarget::Graphics, true),
        ] {
            let mut output = Vec::new();
            render(&mut output, &report, &[], 0, 0, &opts(),
                RenderContext { target, colors: false, width: None }, detail).unwrap();
            assert!(!output.is_empty());
        }
    }
