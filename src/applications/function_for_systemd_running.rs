    // The functions ensure we only queue applications that we've verified we are supposed to run
    // if let Err(err) = resolve_system_applications().await {
    //     log!(
    //         LogLevel::Error,
    //         "Failed to resolve system applications: {}",
    //         err
    //     );
    //     application_controls.signal_shutdown();
    // };

    // if let Err(err) = resolve_client_applications(&config).await {
    //     log!(
    //         LogLevel::Error,
    //         "Failed to resolve client applications, running in safe mode: {}",
    //         err
    //     );
    //     state.config.debug_mode = true;
    //     state.config.environment = "systemonly".to_string();
    // }

    // Spawning applications
    // TODO ADD A GATE FOR IF WE MANAGE SPAWNING APPLICATIONS
    // if state.config.environment != "systemonly" {
    //     spawn_client_applications(
    //         CLIENT_APPLICATION_HANDLER.clone(),
    //         CLIENT_APPLICATION_ARRAY.clone(),
    //         &mut state,
    //         &state_path,
    //     )
    //     .await?;
    // }

    // spawn_system_applications(
    //     SYSTEM_APPLICATION_HANDLER.clone(),
    //     SYSTEM_APPLICATION_ARRAY.clone(),
    //     &mut state,
    //     &state_path,
    // )
    // .await?;