# Test triage (D-53)

The owner asked for fewer unit and end-to-end tests, following the updated
`project-init` Skill: its core validation rules (`core-fast60`) keep the
smallest set of tests that guards each core risk, run end-to-end suites only
on request, and fit one validation batch in 60 seconds. This note records
what changed, what was kept and what was removed, and why. The decision is
D-53 in [DECISIONS.md](DECISIONS.md).

## Result

| | Before | After |
|---|---|---|
| Tests in the default run (`cargo test --workspace --all-features`) | 1742 | 1351 |
| Opt-in suites (`just e2e`, `just hardware`) | 8 hardware | 34 end-to-end + 8 hardware |
| Wall time of the default run (incremental build) | 78.4 s | 18.5 s |

Per crate (default run): `dgxtop-ui` 313 → 176, `dgxtop-manager` 303 → 230,
`dgxtop-adapters` 316 → 253, `dgxtop-store` 274 → 244, `dgxtop-core` 119 → 100,
`dgxtop-collectors` 131 → 122, `dgxtop-runtime` 116 → 102, `dgxtop` 67 → 53,
`tests/action_disposable` 24 → 16, `tests/replay` 6 → 3; `tests/contracts` (35)
and `tests/cli` (17) are unchanged.

## End-to-end suites run on request

These suites start real processes or terminals and take most of the time; they
stay complete and runnable, but outside the default run:

| Suite | Cases | Time | Run with |
|---|---|---|---|
| `tests/pty` (PTY sessions of the real binary) | 7 | 30 s | `cargo test --test pty -- --ignored` |
| `tests/fault_injection` (stuck storage, hung hosts, overload, shutdown) | 13 | 18 s | `cargo test --test fault_injection -- --ignored` |
| `crates/runtime/tests/supervisor_hosts.rs` (real adapter host processes; its own runner now honours `--ignored` and `--skip`) | 14 | 9 s | `cargo test -p dgxtop-runtime --test supervisor_hosts -- --ignored` |

`just e2e` runs all three; `just hardware` still runs the real-hardware smoke
tests. Their evidence is recorded only on request, so the acceptance tests that
rest on them are NOT_RUN (user_opt_in_required) until then: AT06, AT11, AT14,
AT22, AT26, AT30, AT33 and AT39 were PASS while these suites ran by default;
AT01, AT12 and AT24 are NOT_RUN for other reasons too. CI still runs the default
command only: running the opt-in suites there needs a change of the CI workflow,
which needs the owner's request.

## What was kept

- Every check an acceptance test cites in [VALIDATION.md](../validation/VALIDATION.md).
  Two citations pointed at a test removed as a duplicate or renamed earlier; they
  now cite the remaining test of the same invariant. Five cited tests were removed
  by mistake and are back (see [Corrected after the evidence check](#corrected-after-the-evidence-check)).
- The core risks: data integrity (arbitration, counter maths, time-weighted
  history, archive write and read, audit durability, atomic configuration),
  authorization and file boundaries (action capability checks, pidfd identity,
  private files, refused symlinks, path resolution), process lifecycle and
  cleanup (supervision, quarantine, bounded shutdown, terminal restore), public
  CLI, IPC and schema contracts, and distribution metadata (installer checks,
  build provenance).
- The honesty rules of the views: a missing value is never zero, conflicts and
  stale values stay visible, replay data is tagged, results are never claimed
  before they happen.
- Regression tests of fixed bugs (for example the D-50 coverage and REF fixes).

## What was removed and why

- **Layout, colour and wording locks** (most of the UI's removals): whole-screen
  row and column positions, colours of chart columns, help and hint text,
  caption wording, number formatting variants. They broke on every requested
  redesign without guarding a core risk.
- **Duplicates**: the same invariant tested in a unit test and an integration
  test (hwmon, thermal and powercap profile rules, NVML capability re-probing,
  process association merging, registry manifests, SQLite profiles and version
  gate, last-good handling), or twice at one level (paired permission tests,
  token expiry, cancel races already covered by the Manager's process tests,
  the PTY `--version` check also in the CLI suite).
- **Implementation mirrors**: tests that restate a constant or a mapping table
  (signal numbers, level names, status names, a bar glyph, test-helper tests).
- **Peripheral details**: navigation steps, sorting variants, cache timing,
  environment-dependent probes, fixture layout details of the REF generator.

Test-only helpers left unused were removed with them; the action suite's gate
(which held a worker inside a send for the removed cancel-race tests) went with
its last users.

## Corrected after the evidence check

The first pass removed five tests that acceptance tests cite. Against the older
`gate-test` log they still looked present; `scripts/check_validation.py` found
them once the log was recorded again for this tree. They guard core risks, so
they are back, in fewer functions:

| Removed by mistake | Now |
|---|---|
| `process::tests::receipt_arriving_after_the_deadline_but_before_a_tick_does_not_send` (AT32 S3: the deadline re-check when the durable receipt arrives; its end-to-end twin in `at32` was removed as a duplicate) | Restored |
| `store_persist_config`: `temp_creation_failure_keeps_the_old_file`, `file_fsync_failure_keeps_the_old_file` (AT08, AT36) | One table test, `a_failure_before_the_rename_keeps_the_old_file`, also holding the write (`enospc_during_write_keeps_the_old_file`) and rename (`rename_failure_keeps_the_old_file`) cases it replaces |
| `ipc::decode::tests::depth_32_is_accepted_and_33_rejected` (AT33 S3) | `depth_limit_is_exact` checks every depth from 1 to 63 instead of a random sample, so 32 and 33 always run |
| `ipc::frame::tests::byte_at_a_time_including_split_prefix` (AT33 S1) | `random_chunking_preserves_every_frame` also feeds every generated stream one byte at a time |

## Removed tests

By test module (libtest path):

- `action::tests`: `signal_names_and_numbers`
- `arbitration::hysteresis::tests`: `first_selection_is_immediate`
- `arbitration::tests`: `failed_current_source_switches_immediately`, `read_time_alignment_is_flagged_as_weaker_evidence`, `unavailable_round_clears_the_memo`
- `archive::limits::tests`: `from_config_scales_the_wal_budget`
- `at10`: `at10_sigkill_without_allow_force_sends_nothing`
- `at32`: `at32_cancel_after_sending_is_too_late_and_the_single_signal_stands`, `at32_cancel_before_confirmation_wins_and_the_token_is_dead`, `at32_cancel_while_the_intent_is_pending_beats_the_late_durable_receipt`, `at32_durable_receipt_after_the_intent_deadline_never_sends`, `at32_durable_receipt_after_the_token_ttl_never_sends`, `at32_policy_change_revokes_every_unsent_confirmation`, `at32_unknown_send_outcome_is_reported_and_never_retried`
- `(integration test file)`: `at38_cancelled_query_returns_cancelled`, `counter_rates_need_two_samples_then_use_real_elapsed_time`, `deep_identity_and_the_cuda_stack_describe_each_gpu`, `default_options_match_the_brief`, `discovery_describes_interfaces_including_loopback`, `discovery_identifies_cpus_by_label_and_describes_the_host`, `encoded_page_suffix_addresses_the_same_snapshot`, `enospc_during_write_keeps_the_old_file`, `every_collector_consumes_some_fixture_group`, `explanations_are_replaced_per_key_and_other_domains_are_shared`, `fail_fault_fails_every_record_of_the_cycle`, `file_fsync_failure_keeps_the_old_file`, `hang_fault_sleeps_inside_read`, `health_reflects_discovery_state`, `injected_permission_error_yields_no_value`, `interleaved_groups_keep_their_page_snapshots`, `legacy_apply_refuses_a_v2_written_by_a_commit`, `loosely_permitted_legacy_files_import_with_warnings`, `minutes_written_while_the_reader_is_open_are_read_next_time`, `multi_package_x86_pkg_temp_stays_diagnostics`, `negative_temperatures_are_kept`, `per_process_sources_default_to_the_procfs_device_path`, `permission_denied_with_real_file_mode`, `process_group_needs_no_catalog_and_shares_the_clock`, `process_rows_follow_the_procfs_convention`, `processes_are_sampled_once_and_joined_to_gpus`, `ref_slices_follow_the_seven_live_adapter_families`, `reference_temperatures_include_independent_conflicts`, `removals_arrive_only_with_the_complete_page`, `rename_failure_keeps_the_old_file`, `reopening_a_version_2_database_is_idempotent`, `replay_hosts_serve_exactly_their_family_slice`, `rollup_means_equal_raw_means_over_whole_minutes`, `second_instance_is_refused_until_the_first_releases`, `state_paths_follow_xdg_and_keep_replay_separate`, `static_metadata_is_read_once`, `static_metadata_is_read_once_until_the_device_is_recreated`, `temp_creation_failure_keeps_the_old_file`, `unchanged_catalog_keeps_generation_and_parent_resync_gets_everything`, `unchanged_rediscovery_is_empty_and_a_stale_parent_gets_everything`, `unknown_ids_have_no_manifest`, `unsupported_cpufreq_is_reprobed_at_most_once_per_interval`, `validation_rejects_inconsistent_samples_and_faults`, `version_verbose_reports_build_and_sqlite_gate`, `zone_metadata_and_topology_are_read_once`
- `audit::limits::tests`: `from_config_uses_the_configured_budget`
- `audit::records::tests`: `status_names_round_trip`
- `canonical::tests`: `commit_stamps_revision`
- `catalog::tests`: `read_groups_entities_and_capabilities_are_tracked`, `unchanged_pages_do_not_bump_the_revision`
- `cli::tests`: `import_legacy_requires_exactly_one_mode`, `replay_slice_parsing`, `verbose_requires_version`
- `clock::tests`: `unit_helpers`, `utc_clock_is_plausible`
- `commands::tests`: `view_cycle_wraps`
- `config::document::tests`: `defaults_parse`
- `config::repository::tests`: `encoded_form_round_trips_with_trailing_newline`
- `config::tests`: `embedded_defaults_parse_and_validate`, `module_toggles_use_the_registry_and_no_ops_do_not_persist`, `path_helpers_navigate_json`, `persist_failure_keeps_the_old_configuration`, `small_budget_with_tuned_queues_is_valid`
- `context::tests`: `records_where_visits_new_and_retained_records_of_the_wanted_keys_only`
- `control::tests`: `at22_loop_reads_each_group_every_period_and_rediscovers_every_10_s`, `at22_unknown_sources_trigger_rate_limited_rediscovery`, `at23_same_request_id_replays_the_result_and_other_payload_conflicts`, `at30_memory_pressure_throttles_fast_groups_and_frame_rate`, `closing_the_session_revokes_its_tokens_and_closes_descriptors`, `projections_follow_data_changes_within_the_latency_budget_not_the_render_rate`, `public_handles_cross_threads`
- `controller::confirmation::tests`: `a_token_arriving_during_other_input_must_be_cancelled`, `debug_output_never_contains_the_token`
- `controller::keymap::tests`: `digit_keys_follow_the_offered_views`, `hints_and_help_come_from_the_same_table`, `tab_and_shift_tab_variants`, `view_specific_keys`
- `controller::navigation::tests`: `gpu_filter_cycles_all_each_all`, `history_windows_cycle_and_wrap`, `interval_ladder`, `jumps_outside_the_window_use_the_best_known_row_and_keep_a_goal`, `moves_inside_the_window_select_exact_rows`, `no_selection_starts_at_the_first_visible_row_and_empty_lists_do_nothing`, `the_last_row_is_reachable_when_the_window_ends_the_list`
- `controller::requests::tests`: `interval_results_show_configured_and_effective`, `module_toggle_results`
- `controller::session::tests`: `busy_restage_keeps_newer_staged_fields`, `switching_views_remembers_and_restores_view_state`
- `controller::tests`: `a_busy_queue_keeps_the_token_for_another_try`, `a_long_seek_keeps_going_while_each_projection_makes_progress`, `a_rejected_view_change_drops_the_draft`, `busy_navigation_is_retried_on_the_next_batch`, `confirmations_expire_locally`, `g_cycles_the_gpu_filter_through_all_choices`, `h_and_l_switch_gpus_by_entity`, `help_opens_scrolls_and_closes`, `history_windows_step_and_there_is_no_series_to_pick`, `home_and_end_reach_the_first_and_last_rows_beyond_the_window`, `k_works_from_the_overview_process_panel`, `navigation_bursts_coalesce_and_build_on_each_other`, `page_down_moves_by_the_visible_page`, `settings_space_toggles_the_selected_module`, `sort_and_order_keys_update_the_session`, `sources_navigation_and_filter_target`, `switching_views_restores_the_process_filter`, `version_changes_with_visible_state_only`
- `cpu::tests`: `declared_energy_modulus_wraps_once`, `frequency_load_and_tasks_pass_through`, `sensor_channels_appear_only_in_explanations`
- `event::tests`: `action_events_are_control`
- `events::tests`: `recent_returns_newest_in_order`
- `format::age::tests`: `intervals_and_spans`
- `format::cell::tests`: `full_detail_includes_age`, `short_labels_fit_narrow_columns`, `value_with_source_and_quality_word`
- `format::text::tests`: `padding_is_exact`
- `format::units::tests`: `durations_read_in_the_largest_fitting_unit`, `energy_reads_in_watt_hours_then_kilowatt_hours`, `peak_bandwidths_read_like_the_data_sheet`
- `fs::failpoint::tests`: `uid_override_and_disarm`
- `gpu::tests`: `ecc_counts_pass_through`
- `history::output::tests`: `drops_are_counted_per_reason`
- `history::tests`: `a_run_longer_than_the_window_reads_only_the_windows_that_reach_back`, `a_window_change_redraws_the_charts_and_keeps_the_statistics`, `five_minutes_skip_the_maxima_and_refresh_every_five_seconds`, `helpers_are_bounded`, `throttled_history_refreshes_three_times_slower`
- `host::tests`: `host_generation_argument_parsing`
- `hot::counters::tests`: `reports_are_folded_into_counters`
- `hot::estimate::tests`: `estimates_grow_with_content`
- `hot::snapshot::tests`: `missing_value_keeps_the_previous_last_good`
- `hwmon::profile::tests`: `amd_tdie_preferred_and_tctl_only_without_tdie`, `coretemp_package_and_cores_map_with_confirmed_origins`, `gb10_clusters_and_platform_are_usable_not_verified`, `unknown_drivers_are_diagnostics_with_unknown_origin`
- `hwmon::sysfs::tests`: `live_manifest_is_valid`
- `identity::tests`: `decoding_shares_repeated_identities_and_keeps_validating`
- `ingestion::pages::tests`: `single_page_cycles_complete_immediately`
- `ingestion::tests`: `inputs_of_a_busy_adapter_wait_briefly_for_the_rest_of_its_cycle`
- `ingestion::trends::tests`: `disk_and_network_rates_are_tracked_per_device`, `gpu_memory_is_tracked_per_gpu_only`
- `ipc::decode::tests`: `brackets_inside_strings_and_escapes_do_not_count`, `depth_32_is_accepted_and_33_rejected`
- `ipc::frame::tests`: `byte_at_a_time_including_split_prefix`
- `ipc::size::tests`: `estimate_grows_with_records_and_strings`
- `logger::coalesce::tests`: `a_new_window_carries_the_previous_summary`
- `logger::file::tests`: `identical_records_are_coalesced_with_a_summary`
- `logger::format::tests`: `level_names`
- `metric::tests`: `every_catalog_entry_is_found_by_its_id_and_nothing_else_is`, `u64_scalar_round_trips`
- `modules::common::tests`: `a_bar_row_puts_the_value_after_its_gauge_in_its_level_color`, `bar_rows_line_their_gauges_up_and_fit_tiny_rows`, `columns_drop_by_priority_and_flex_absorbs_leftover`, `optional_columns_yield_to_starved_important_columns`, `row_cursor_hands_out_rows_until_exhausted`, `two_lines_share_a_row_only_when_both_fit`
- `modules::cpu::tests`: `grid_prefers_two_columns_and_respects_width`, `sensor_labels_do_not_repeat_their_prefix`
- `modules::disk::tests`: `without_devices_or_filesystems_there_are_no_rows`
- `modules::gpu::blocks::tests`: `every_block_reads_top_to_bottom_like_a_spec_sheet`, `the_compute_capability_names_its_nvcc_target`
- `modules::gpu::columns::tests`: `keys_align_down_a_column_and_tables_name_their_values`, `the_fewest_columns_that_fit_take_the_blocks_balanced`
- `modules::gpu::facts::tests`: `the_metadata_and_the_details_answer_alike`, `throttle_reasons_take_the_color_of_the_worst`
- `modules::gpu::readings::tests`: `a_bar_puts_its_value_after_the_gauge_and_its_chart_under_it`
- `modules::gpu::tests`: `readings_of_one_kind_share_a_width_and_all_rows_one_column`
- `modules::gpu::view::tests`: `every_block_shows_under_a_separator_when_they_fit_with_it`
- `modules::history::axis::tests`: `labels_sit_under_their_ticks_and_never_touch`
- `modules::history::layout::tests`: `a_tall_terminal_caps_the_strips_and_the_first_band`, `a_wide_terminal_pairs_the_panels_and_fills_its_rows`
- `modules::history::panels::tests`: `a_view_without_charts_has_no_panels`
- `modules::history::stats::tests`: `a_narrow_panel_drops_the_energy_and_shortens_the_gpu`, `each_gpu_lists_its_windows_with_mean_and_peak_and_energy`, `every_window_of_every_gpu_has_a_label`
- `modules::history::strip::tests`: `a_lone_strip_shows_its_scale_its_columns_and_now_avg_max`, `a_strip_of_several_names_itself_and_power_adds_its_energy`, `scales_fit_the_gutter`
- `modules::history::tests`: `hidden_parts_are_named_in_ever_shorter_forms`, `the_bar_notes_shorten_from_the_end_keeping_what_did_not_fit`
- `modules::network::tests`: `up_interfaces_get_a_filled_dot`
- `modules::processes::tests`: `display_window_centres_the_selection_and_clamps_at_both_ends`
- `modules::settings::tests`: `persistence_text_includes_usage_and_detail`
- `modules::sources::tests`: `selection_falls_back_to_session_then_first`
- `monitor::tests`: `a_flag_set_later_is_observed_before_the_deadline`, `a_set_flag_ends_the_wait_at_once`
- `network::interfaces::tests`: `counters_map_to_metric_ids_and_units`
- `nvml::capabilities::tests`: `transient_errors_are_not_cached_and_success_clears`, `unsupported_is_reprobed_at_most_once_per_interval`
- `nvml::channels::tests`: `deep_channels_are_slow_and_static_ones_cached`, `pcie_is_marked_unverified`
- `nvml::library::tests`: `compute_modes_have_stable_names`, `identity_fields_read_like_nvidia_smi`
- `nvml::processes::tests`: `compute_and_graphics_of_one_pid_merge_without_double_counting`, `unavailable_memory_never_becomes_zero_and_known_value_wins`
- `nvml::stack::tests`: `a_relative_default_link_and_a_bare_cuda_directory_resolve`, `toolkits_list_the_default_first_then_the_newest_with_their_nvcc`
- `overload::tests`: `ingress_full_must_persist_before_throttling`
- `paths::tests`: `overrides_do_not_need_home`
- `perf::coverage::tests`: `replay_groups_follow_the_live_period_classes`
- `perf::environment::tests`: `longest_mount_prefix_wins_and_escapes_are_decoded`, `nvidia_version_and_truncation`
- `perf::evidence::tests`: `default_and_explicit_report_paths`
- `perf::fixture::tests`: `slices_partition_every_read_group_over_seven_hosts`
- `perf::scenarios::tests`: `replay_profile_carries_the_scenario_counts`
- `perf::tests`: `nearest_rank_matches_spec_definition`
- `perf::threads::tests`: `harness_threads_publish_their_own_cpu`
- `policy::tests`: `initial_snapshot_mirrors_config_and_defaults`
- `ports::tests`: `cancel_flag_is_shared`
- `powercap::profile::tests`: `package_and_psys_are_distinct_metrics`, `subdomains_mmio_and_dies_are_diagnostics`, `zone_hierarchy_helpers`
- `presentation::banners::tests`: `conflicts_are_counted_across_domains`, `healthy_system_has_no_banners`
- `presentation::budget::tests`: `disk_and_network_chart_trends_are_shed_like_the_others`
- `presentation::cell::tests`: `conflict_between_two_sources_of_one_adapter_uses_source_ids`, `kind_and_quality_are_carried`, `unavailable_states_are_derived_from_candidates`
- `presentation::cpu::tests`: `duplicate_scopes_name_their_entity`, `frequency_max_prefers_host_then_highest_core`, `model_comes_from_catalog_then_host_facts`, `package_soc_and_power_rows_keep_distinct_labels`, `utilization_trend_uses_the_trend_book`
- `presentation::disk::tests`: `filesystems_use_catalog_attributes_sorted_by_mount_point`, `without_devices_the_trends_are_empty`
- `presentation::gpu::tests`: `capabilities_and_slow_health_fill_the_card`, `clocks_list_current_domains_and_reported_maxima`, `selection_and_trend_widths_follow_the_requested_card`, `unsupported_state_thresholds_and_link_stay_unsupported_never_zero`
- `presentation::gpu_directory::tests`: `non_gpu_entities_are_ignored`
- `presentation::history::tests`: `mixed_methods_become_time_weighted_steps`
- `presentation::history_charts::tests`: `the_disk_shown_stays_while_it_reports_then_the_busiest_takes_over`
- `presentation::labels::tests`: `host_state_text_includes_detail`, `needle_is_case_insensitive_and_empty_matches_all`, `origin_and_source_helpers`, `scopes_read_as_words_and_soc_is_capitalized`
- `presentation::memory::tests`: `discrete_gpus_only_report_no_gpu_ram`
- `presentation::network::tests`: `a_trend_is_cut_to_the_requested_width`
- `presentation::plan::tests`: `disabled_modules_are_not_shown_but_reported`
- `presentation::processes::tests`: `scrolling_to_the_last_row_copies_only_window_and_overscan`, `ties_are_broken_by_process_key_in_both_directions`, `visible_rows_follow_the_reference_grid`
- `presentation::reader::tests`: `trends_resolve_keys_with_or_without_samples`
- `presentation::settings::tests`: `overload_notes_attach_to_the_group_field`, `pending_result_modules_and_status_are_carried`
- `presentation::sources::tests`: `gpu_series_name_their_gpu`, `unclassified_sensors_come_from_explanations_and_catalog`, `window_respects_candidate_budget_and_keeps_center`
- `presentation::tests`: `disabled_module_stays_empty_and_is_announced`, `gpu_detail_gives_full_trends_to_the_selected_card_only`, `the_overview_copies_only_the_gpu_trends_it_draws`
- `process::record::tests`: `revoke_reasons_map_to_codes`
- `process::tests`: `an_outstanding_exit_poll_is_not_an_overdue_deadline`
- `procfs::cpu::tests`: `frequency_summary_reflects_support`
- `procfs::disk::tests`: `ram_and_loop_devices_are_skipped`
- `procfs::host::tests`: `sysconf_constants_are_positive_when_known`
- `procfs::mechanics::tests`: `sticky_failures_are_reprobed_once_per_interval`, `transient_errors_are_not_sticky`
- `procfs::memory::tests`: `absent_fields_become_unsupported_capabilities`
- `procfs::parse::tests`: `cpu_model_from_model_name_only`
- `projection::tests`: `frame_interval_follows_fps`, `projections_are_data_driven_only_in_the_normal_state`
- `query::points::tests`: `empty_spans_are_ignored`
- `registry::tests`: `default_options_match_the_contract`, `detail_views_map_to_modules`, `every_live_adapter_has_a_valid_manifest`, `unknown_and_replay_ids_are_rejected`
- `replay::adapter::tests`: `counters_advance_across_loops_and_respect_modulus`
- `replay::generator::tests`: `round_robin_covers_every_family`
- `scheduler::tests`: `a_passed_retry_instant_never_becomes_an_overdue_deadline`, `multiplier_changes_fast_periods_without_a_new_epoch`
- `session::tests`: `viewport_round_trips_and_defaults`
- `shared_text::tests`: `behaves_like_a_string`, `clones_and_repeated_decodes_share_one_allocation`
- `shutdown::tests`: `default_reason_when_none_recorded`
- `sqlite::capacity::tests`: `sibling_paths`
- `sqlite::connection::tests`: `archive_profile_is_wal_normal`, `audit_profile_is_wal_full_with_foreign_keys`
- `sqlite::version::tests`: `build_and_runtime_must_match`, `bundled_sqlite_passes_the_gate`, `messages_use_dotted_versions`
- `startup::tests`: `differences_list_only_changed_leaves`
- `stats::tests`: `own_process_has_every_reading`
- `supervisor::config::tests`: `built_from_the_application_config`
- `supervisor::credit::tests`: `small_plans_reserve_small_credit`
- `supervisor::launch::tests`: `exit_summaries_render`
- `terminal::backend::tests`: `modifier_changes_are_minimal`, `size_and_cursor_are_tracked_locally`
- `terminal::clock::tests`: `system_clock_is_monotonic`
- `terminal::input::tests`: `buffered_events_and_resizes_are_delivered_without_waiting`, `idle_terminal_waits_for_the_whole_timeout`, `input_arriving_during_the_wait_is_read`, `zero_timeout_never_waits`
- `terminal::runner::tests`: `resize_updates_the_session_viewport_and_redraws`, `the_first_frame_follows_the_enter_sequence`
- `terminal::scheduler::tests`: `first_frame_draws_then_nothing_without_changes_until_the_age_tick`
- `terminal::stats::tests`: `summary_copies_counters`
- `terminal::tests`: `idle_pipe_times_out_after_the_timeout`
- `testing::tests`: `capturing_dispatch_can_be_busy`, `fake_clock_steps_utc_independently`
- `text::tests`: `basename_takes_last_component`, `command_names_keep_slashes`
- `theme::tests`: `every_theme_has_a_distinct_palette`
- `thermal::profile::tests`: `arm_cpu_zone_is_usable_rank_two_only_with_one_package`, `other_zones_are_diagnostics`, `soc_zones_map_to_soc_temperature`, `x86_pkg_temp_needs_one_package_and_one_zone`
- `time_format::tests`: `end_of_year_rolls_over`, `instants_before_1970_use_the_right_calendar_day`
- `view::tests`: `history_plots_take_the_panel_less_its_gutter_and_legend`
- `viewer::banners::tests`: `narrow_confirmation_fits_between_header_and_footer_at_40x10`, `no_banner_when_nothing_is_pending`, `word_wrap_is_exact_and_breaks_long_words`
- `viewer::chrome::tests`: `filter_display_keeps_the_cursor_visible`, `header_segments_keep_priorities_when_narrow`
- `viewer::help::tests`: `every_binding_appears_in_help`
- `viewer::isolation::tests`: `non_string_payloads_have_a_generic_message`
- `viewer::layout::tests`: `a_row_that_cannot_sit_side_by_side_is_stacked`, `an_expanded_module_grows_its_row_first`, `detail_views_place_the_module_after_a_summary`, `minimal_overview_is_summary_only`, `rows_grow_by_priority_gpus_first_and_the_filling_row_keeps_its_minimum`, `the_lowest_priority_rows_are_hidden_when_minimums_do_not_fit`, `the_overview_follows_the_reference_grid`, `the_wide_tiers_draw_tabs_in_the_header_row`, `widths_follow_the_weights_and_keep_each_minimum`, `without_cells_the_overview_stacks_by_priority`
- `viewer::summary::tests`: `detail_views_show_only_carried_topics_other_than_their_own`, `lines_cover_every_topic_with_state_text`, `narrow_form_splits_topics_into_short_lines`, `one_line_form_is_compact`
- `viewer::tests`: `a_calm_header_keeps_only_host_facts_and_flags_an_old_projection`, `a_short_gpu_view_shows_whole_blocks_and_names_the_rest`, `a_short_wide_terminal_with_two_gpus_keeps_disk_and_process_rows`, `disabled_modules_are_hidden_from_the_overview`, `every_history_column_takes_the_gauge_color_of_its_level`, `every_theme_renders`, `filter_mode_footer_shows_buffer_cursor_and_ctrl_c_quit`, `help_overlay_is_generated_from_the_keymap`, `history_is_a_dashboard_of_the_overview_resources_on_one_time_axis`, `memory_marks_the_ram_gpu_processes_hold_in_its_own_color`, `minimal_overview_is_a_summary_with_navigation`, `notices_and_registry_warnings_are_banners`, `overview_key_texts_at_medium_sizes`, `resize_churn_never_panics`, `the_axis_spans_the_window_the_manager_charts`, `the_cpu_starts_collapsed_and_expands_to_cores_and_sensors`, `the_gpu_view_is_live_bars_over_blocks_of_information`, `the_gpu_view_of_a_unified_gpu_shows_the_ram_it_holds_and_the_driver_traffic`, `the_wide_header_carries_clickable_tabs`, `waiting_screen_before_the_first_projection`
- `views::tests`: `tab_cycling_wraps_over_offered_views_only`
- `widgets::bar::tests`: `a_bar_stands_three_quarters_of_a_row_high`
- `widgets::sparkline::tests`: `keeps_newest_points_right_aligned`, `level_colors_follow_each_column_like_a_gauge`, `peaks_take_their_own_style_when_given_one`
- `widgets::stacked_bar::tests`: `an_empty_area_draws_nothing`
- `wire::tests`: `fixed_width_preserves_order`
