    use super::*;

    const TCP4: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 34567 1 0000000000000000 100 0 0 10 0\n   1: 0501A8C0:AD22 5DF36B22:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 34568 1 0000000000000000 100 0 0 10 0\n   2: 0100007F:0016 00000000:0000 06 00000000:00000000 00:00000000 00000000     0        0 0 1 0000000000000000 100 0 0 10 0\n";

    const TCP6: &str = "  sl  local_address rem_address st ...\n   0: 00000000000000000000000001000000:0050 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 45678 1\n   1: 0000000000000000FFFF00000100007F:01BB 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 45679 1\n";

    const UDP4: &str = "  sl  local_address rem_address st ...\n  71: 00000000:0044 00000000:0000 07 00000000:00000000 00:00000000 00000000   998        0 23456 2\n";

    fn p(pid: i32, ppid: i32, comm: &str, cmd: &[&str], cpu: f64, rss: u64) -> crate::pq::proc::Proc {
        crate::pq::proc::Proc {
            pid, ppid, comm: comm.into(), cmdline: cmd.iter().map(|s| s.to_string()).collect(),
            rss, swap: 0, uid: 0, cpu_pct: cpu
        }
    }

    fn gradle_and_postgres() -> Vec<crate::pq::proc::Proc> {
        vec![
            p(100, 1, "java", &["java", "org.gradle...GradleDaemon"], 10.0, 1000),
            p(101, 100, "java", &["java", "-cp", "/tmp/worker.jar"], 20.0, 2000),
            p(200, 1, "postgres", &["postgres", "-D", "/data"], 5.0, 500),
        ]
    }

    #[test]
    fn sockets_join_identity_clusters() {
        let mut estab = tsock(44000, SockState::Established, 1000, 12);
        estab.peer_port = 443;
        let attributed = vec![
            (tsock(8080, SockState::Listen, 1000, 10), Some(101)), // gradle child
            (estab, Some(100)),                                    // gradle daemon
            (tsock(5432, SockState::Listen, 26, 11), Some(200)),   // postgres
        ];
        let r = net_clusters(&gradle_and_postgres(), attributed, None, HashMap::new());
        assert_eq!(r.total_conns, 3);
        assert_eq!(r.total_listening, 2);
        assert_eq!(r.unattributed_count, 0);
        assert_eq!(r.clusters.len(), 2);
        // gradle first: 2 conns beats postgres's 1.
        assert_eq!(r.clusters[0].identity, "gradle");
        assert_eq!(r.clusters[0].conns.len(), 2);
        assert_eq!(r.clusters[0].listen_ports, vec![8080]);
        assert_eq!(r.clusters[0].proc_count, 2);
        assert!((r.clusters[0].cpu - 30.0).abs() < 0.01);
        // Listeners sort before established within a cluster.
        assert!(r.clusters[0].conns[0].0.is_listening());
        assert_eq!(r.clusters[1].identity, "postgres");
    }

    #[test]
    fn socketless_clusters_are_dropped_and_unattributed_group_by_uid() {
        let attributed = vec![
            (tsock(80, SockState::Listen, 33, 20), None),
            (tsock(81, SockState::Listen, 33, 21), None),
        ];
        let r = net_clusters(&gradle_and_postgres(), attributed, None, HashMap::new());
        assert!(r.clusters.is_empty()); // no process owned a socket
        assert_eq!(r.unattributed.len(), 1);
        assert_eq!(r.unattributed[0].0, 33);
        assert_eq!(r.unattributed[0].1.len(), 2);
        assert_eq!(r.unattributed_count, 2);
        assert_eq!(r.total_conns, 2);
    }

    #[test]
    fn pattern_filters_clusters_but_totals_stay_global() {
        let attributed = vec![
            (tsock(8080, SockState::Listen, 1000, 10), Some(100)),
            (tsock(5432, SockState::Listen, 26, 11), Some(200)),
            (tsock(99, SockState::Listen, 33, 12), None),
        ];
        let r = net_clusters(&gradle_and_postgres(), attributed, Some("postgres"), HashMap::new());
        assert_eq!(r.clusters.len(), 1);
        assert_eq!(r.clusters[0].identity, "postgres");
        // Unattributed rows cannot match a pattern: hidden from display, still counted.
        assert!(r.unattributed.is_empty());
        assert_eq!(r.unattributed_count, 1);
        assert_eq!(r.total_conns, 3); // header totals are pre-pattern, like pq's cpu total today
    }

    #[test]
    fn clusters_sort_by_traffic_before_conn_count() {
        let mut heavy = tsock(50000, SockState::Established, 1000, 77);
        heavy.peer_port = 443;
        let attributed = vec![
            // gradle: two sockets, zero bytes; postgres: one socket, 5MB moved.
            (tsock(8080, SockState::Listen, 1000, 10), Some(100)),
            (tsock(8081, SockState::Listen, 1000, 11), Some(100)),
            (heavy, Some(200)),
        ];
        let traffic: HashMap<u64, (u64, u64)> = [(77u64, (5_000_000u64, 1_000u64))].into_iter().collect();
        let r = net_clusters(&gradle_and_postgres(), attributed, None, traffic);
        assert_eq!(r.clusters[0].identity, "postgres"); // bytes beat connection count
        assert_eq!(r.clusters[0].traffic(&r.traffic), (5_000_000, 1_000));
    }

    #[test]
    fn direction_and_world_exposure() {
        let listen: HashSet<u16> = [80u16].into_iter().collect();
        let lsn = tsock(80, SockState::Listen, 0, 1);
        assert_eq!(lsn.direction(&listen), "listen");
        assert!(!lsn.is_world_exposed()); // tsock binds 127.0.0.1, loopback only

        let mut inbound = tsock(80, SockState::Established, 0, 2);
        inbound.peer_port = 55555;
        assert_eq!(inbound.direction(&listen), "in"); // accepted on a listened port

        let mut outbound = tsock(44000, SockState::Established, 0, 3);
        outbound.peer_port = 443;
        assert_eq!(outbound.direction(&listen), "out"); // ephemeral local port

        let mut world = tsock(80, SockState::Listen, 0, 4);
        world.local = "0.0.0.0".parse().unwrap();
        assert!(world.is_world_exposed());
    }

    #[test]
    fn net_clusters_computes_listen_set_and_world_totals() {
        let mut world = tsock(8080, SockState::Listen, 1000, 10);
        world.local = "0.0.0.0".parse().unwrap();
        let attributed = vec![
            (world, Some(100)),                                  // gradle, world-exposed
            (tsock(5432, SockState::Listen, 26, 11), Some(200)), // postgres, loopback only
        ];
        let r = net_clusters(&gradle_and_postgres(), attributed, None, HashMap::new());
        assert_eq!(r.total_world, 1);
        assert!(r.listen_port_set.contains(&8080) && r.listen_port_set.contains(&5432));
        let gradle = r.clusters.iter().find(|c| c.identity == "gradle").unwrap();
        assert_eq!(gradle.world_ports, vec![8080]);
        assert_eq!(gradle.listeners(), 1);
        assert_eq!(gradle.established(), 0);
        let pg = r.clusters.iter().find(|c| c.identity == "postgres").unwrap();
        assert!(pg.world_ports.is_empty());
    }

    #[test]
    fn port_kill_candidates_selects_only_attributed_listeners() {
        let mut estab = tsock(8080, SockState::Established, 1000, 2);
        estab.peer_port = 55555;
        // All listeners on the port are attributed: proceed with every owning pid.
        let all_attributed = vec![
            (tsock(8080, SockState::Listen, 1000, 1), Some(4242)),
            (tsock(8080, SockState::Listen, 1000, 9), Some(4242)),
            (estab.clone(), Some(777)),
        ];
        match port_kill_candidates(&all_attributed, 8080) {
            PortCandidates::Listeners(owners) => {
                assert_eq!(owners.len(), 2);
                assert!(owners.iter().all(|(_, pid)| *pid == 4242));
            }
            other => panic!("expected Listeners, got {:?}", other)
        }
        // A mixed attributed+unattributed port is a hard error: the unattributed listener likely
        // belongs to a different process that would silently survive the kill.
        let mixed = vec![
            (tsock(8080, SockState::Listen, 1000, 1), Some(4242)),
            (tsock(8080, SockState::Listen, 1000, 9), None),
            (estab.clone(), Some(777)),
        ];
        assert!(matches!(port_kill_candidates(&mixed, 8080), PortCandidates::Unattributable { uid: 1000 }));
        // Listener exists but nothing attributed: hard stop with the owning uid.
        let unowned = vec![(tsock(8080, SockState::Listen, 990, 1), None)];
        assert!(matches!(port_kill_candidates(&unowned, 8080), PortCandidates::Unattributable { uid: 990 }));
        // No listener at all: the established socket comes back for the diagnostic listing.
        let no_listener = vec![(estab, Some(777))];
        match port_kill_candidates(&no_listener, 8080) {
            PortCandidates::NoListener(others) => assert_eq!(others.len(), 1),
            other => panic!("expected NoListener, got {:?}", other)
        }
        assert!(matches!(port_kill_candidates(&[], 8080), PortCandidates::NoListener(v) if v.is_empty()));
    }

    #[test]
    fn parses_tcp4_listener_and_established() {
        let socks = parse_net_table(TCP4, Proto::Tcp);
        assert_eq!(socks.len(), 2); // the st=06 (TIME_WAIT) inode-0 row is dropped
        let listen = &socks[0];
        assert_eq!(listen.local, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(listen.local_port, 8080); // 0x1F90
        assert_eq!(listen.state, SockState::Listen);
        assert_eq!(listen.uid, 1000);
        assert_eq!(listen.inode, 34567);
        assert!(listen.is_listening());
        let estab = &socks[1];
        assert_eq!(estab.local, "192.168.1.5".parse::<IpAddr>().unwrap()); // 0501A8C0 LE
        assert_eq!(estab.local_port, 44322); // 0xAD22
        assert_eq!(estab.peer, "34.107.243.93".parse::<IpAddr>().unwrap()); // 5DF36B22 LE
        assert_eq!(estab.peer_port, 443);
        assert_eq!(estab.state, SockState::Established);
        assert!(!estab.is_listening());
    }

    #[test]
    fn parses_tcp6_and_collapses_v4_mapped() {
        let socks = parse_net_table(TCP6, Proto::Tcp);
        assert_eq!(socks.len(), 2);
        assert_eq!(socks[0].local, "::1".parse::<IpAddr>().unwrap());
        assert_eq!(socks[0].local_port, 80);
        // ::ffff:127.0.0.1 collapses to plain 127.0.0.1 so --port and display treat it as v4.
        assert_eq!(socks[1].local, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(socks[1].local_port, 443);
    }

    #[test]
    fn udp_bound_socket_is_listening() {
        let socks = parse_net_table(UDP4, Proto::Udp);
        assert_eq!(socks.len(), 1);
        assert_eq!(socks[0].local_port, 68); // 0x0044
        assert_eq!(socks[0].state, SockState::Other(7));
        assert!(socks[0].is_listening());
        assert_eq!(socks[0].uid, 998);
    }

    #[test]
    fn labels_render_for_listener_and_peer() {
        let socks = parse_net_table(TCP4, Proto::Tcp);
        assert_eq!(socks[0].label(), "tcp LISTEN  127.0.0.1:8080");
        assert_eq!(socks[1].label(), "tcp ESTAB  192.168.1.5:44322 -> 34.107.243.93:443");
        assert_eq!(socks[0].peer_str(), "");
    }

    #[test]
    fn garbage_lines_are_skipped() {
        assert!(parse_net_table("header\nnot a socket line\n", Proto::Tcp).is_empty());
        assert!(parse_net_table("", Proto::Tcp).is_empty());
    }

    /// Test builder: a TCP socket on 127.0.0.1 with the given local port, state, uid, inode.
    fn tsock(port: u16, state: SockState, uid: u32, inode: u64) -> Sock {
        Sock {
            proto: Proto::Tcp, state,
            local: "127.0.0.1".parse().unwrap(), local_port: port,
            peer: "0.0.0.0".parse().unwrap(), peer_port: 0,
            uid, inode
        }
    }

    #[test]
    fn attribute_joins_by_inode() {
        let socks = vec![tsock(80, SockState::Listen, 0, 111), tsock(81, SockState::Listen, 33, 999)];
        let inodes: HashMap<u64, i32> = [(111u64, 4242i32)].into_iter().collect();
        let attributed = attribute(socks, &inodes);
        assert_eq!(attributed[0].1, Some(4242));
        assert_eq!(attributed[1].1, None);
    }

    #[test]
    fn filter_socks_by_port_and_listen() {
        let mut estab = tsock(8080, SockState::Established, 0, 3);
        estab.peer_port = 55555;
        let socks = vec![
            (tsock(8080, SockState::Listen, 0, 1), Some(1)),
            (tsock(443, SockState::Listen, 0, 2), Some(2)),
            (estab, Some(3)),
        ];
        let by_port = filter_socks(socks.clone(), Some(8080), false);
        assert_eq!(by_port.len(), 2); // listener AND established both have local port 8080
        let listening = filter_socks(socks.clone(), Some(8080), true);
        assert_eq!(listening.len(), 1);
        assert_eq!(filter_socks(socks, None, false).len(), 3);
    }

    #[test]
    fn extended_filters_compose() {
        let socks = vec![
            (Sock { proto: Proto::Tcp, state: SockState::Established,
                local: "127.0.0.1".parse().unwrap(), local_port: 40000,
                peer: "1.1.1.1".parse().unwrap(), peer_port: 443, uid: 1000, inode: 1 }, Some(10)),
            (Sock { proto: Proto::Udp, state: SockState::Other(7),
                local: "0.0.0.0".parse().unwrap(), local_port: 53,
                peer: "0.0.0.0".parse().unwrap(), peer_port: 0, uid: 0, inode: 2 }, Some(20)),
        ];
        assert_eq!(filter_socks_extended(socks.clone(), Some(443), Some(Proto::Tcp), Some("established"), false).len(), 1);
        assert_eq!(filter_socks_extended(socks.clone(), None, Some(Proto::Udp), Some("bound"), true).len(), 1);
        assert!(filter_socks_extended(socks, None, Some(Proto::Tcp), Some("listen"), false).is_empty());
    }

    #[test]
    fn finds_own_listener_attributed_to_self() {
        // Live end-to-end: bind an ephemeral listener, then find it in /proc/net attributed to
        // this very process. Only touches our own pid, so it runs unprivileged.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id() as i32;
        let (socks, tables_read) = read_sockets();
        assert!(tables_read > 0, "no /proc/net table readable");
        let inodes = inode_map(&[me]);
        let attributed = attribute(socks, &inodes);
        assert!(
            attributed.iter().any(|(s, pid)| s.local_port == port && s.is_listening() && *pid == Some(me)),
            "own listener on port {port} not found or not attributed"
        );
    }
