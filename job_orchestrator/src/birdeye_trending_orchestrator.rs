use anyhow::Result;
use config_manager::SystemConfig;
use dex_client::{BirdEyeClient, TopTrader, TrendingToken as BirdEyeTrendingToken, GeneralTraderTransaction, GainerLoser, DexScreenerClient, DexScreenerBoostedToken, NewListingToken, NewListingTokenFilter};
use persistence_layer::{RedisClient, DiscoveredWalletToken};
// NewFinancialEvent/NewEventType imports removed - using GeneralTraderTransaction directly
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

// BirdEyeTrendingConfig removed - now uses SystemConfig directly

/// Orchestrates trending token discovery and top trader identification using BirdEye API + DexScreener boosted tokens
pub struct BirdEyeTrendingOrchestrator {
    config: SystemConfig,
    birdeye_client: BirdEyeClient,
    dexscreener_client: Option<DexScreenerClient>,
    redis_client: Arc<Mutex<Option<RedisClient>>>,
    is_running: Arc<Mutex<bool>>,
}

impl BirdEyeTrendingOrchestrator {
    /// Create a new BirdEye trending orchestrator
    pub fn new(
        config: SystemConfig,
        redis_client: Option<RedisClient>,
    ) -> Result<Self> {
        // Use BirdEye config from SystemConfig
        let birdeye_config = config.birdeye.clone();
        let birdeye_client = BirdEyeClient::new(birdeye_config)?;
        
        // Initialize DexScreener client if enabled
        let dexscreener_client = if config.dexscreener.enabled {
            let dexscreener_config = dex_client::DexScreenerClientConfig {
                api_base_url: config.dexscreener.api_base_url.clone(),
                request_timeout_seconds: config.dexscreener.request_timeout_seconds,
                rate_limit_delay_ms: config.dexscreener.rate_limit_delay_ms,
                max_retries: config.dexscreener.max_retries,
                enabled: config.dexscreener.enabled,
            };
            Some(DexScreenerClient::new(dexscreener_config)?)
        } else {
            None
        };
        
        Ok(Self {
            config,
            birdeye_client,
            dexscreener_client,
            redis_client: Arc::new(Mutex::new(redis_client)),
            is_running: Arc::new(Mutex::new(false)),
        })
    }

    /// Start the trending discovery loop
    pub async fn start(&self) -> Result<()> {
        let mut is_running = self.is_running.lock().await;
        if *is_running {
            warn!("BirdEye trending orchestrator is already running");
            return Ok(());
        }
        *is_running = true;
        drop(is_running);

        info!("🚀 Starting Enhanced Multi-Sort BirdEye Discovery Orchestrator");
        info!("📋 Enhanced Discovery: 3 sorting strategies (rank + volume + liquidity), unlimited tokens, max_traders_per_token={}, cycle_interval={}s", 
              self.config.birdeye.max_traders_per_token, 60);

        loop {
            // Check if we should stop
            {
                let is_running = self.is_running.lock().await;
                if !*is_running {
                    info!("🛑 BirdEye trending orchestrator stopped");
                    break;
                }
            }

            // Execute one cycle
            match self.execute_discovery_cycle().await {
                Ok(discovered_wallets) => {
                    if discovered_wallets > 0 {
                        info!("✅ Cycle completed: discovered {} quality wallets", discovered_wallets);
                    } else {
                        debug!("🔍 Cycle completed: no new quality wallets discovered");
                    }
                }
                Err(e) => {
                    error!("❌ Discovery cycle failed: {}", e);
                }
            }

            // Wait before next cycle (interruptible sleep)
            let sleep_duration = Duration::from_secs(60); // BirdEye polling interval
            let mut interval = tokio::time::interval(Duration::from_millis(500)); // Check stop flag every 500ms
            let start_time = std::time::Instant::now();
            
            loop {
                interval.tick().await;
                
                // Check if we should stop during sleep
                {
                    let is_running = self.is_running.lock().await;
                    if !*is_running {
                        info!("🛑 Stop requested during sleep, breaking out early");
                        return Ok(());
                    }
                }
                
                // Check if we've slept long enough
                if start_time.elapsed() >= sleep_duration {
                    break;
                }
            }
        }

        Ok(())
    }

    /// Stop the trending discovery loop
    pub async fn stop(&self) {
        let mut is_running = self.is_running.lock().await;
        *is_running = false;
        info!("🛑 BirdEye trending orchestrator stop requested");
    }

    /// Execute one complete discovery cycle with enhanced multi-source strategy
    pub async fn execute_discovery_cycle(&self) -> Result<usize> {
        // Set is_running to true for this cycle
        {
            let mut is_running = self.is_running.lock().await;
            *is_running = true;
        }
        
        info!("🔄 Starting Enhanced Multichain Discovery Cycle");
        debug!("📊 Discovery sources: 1) Paginated trending tokens (unlimited), 2) Paginated gainers (3 timeframes), 3) DexScreener boosted");
        
        let mut total_discovered_wallets = 0;
        
        // Iterate through all enabled chains
        for chain in &self.config.multichain.enabled_chains {
            info!("🔗 Processing chain: {}", chain);
            
            total_discovered_wallets += self.execute_discovery_cycle_for_chain(chain).await?;
            
            // Check if we should stop between chains
            {
                let is_running = self.is_running.lock().await;
                if !*is_running {
                    info!("🛑 Stop requested between chains, breaking out");
                    break;
                }
            }
        }
        
        info!("✅ Multichain discovery cycle completed: {} total wallets discovered across {} chains", 
              total_discovered_wallets, self.config.multichain.enabled_chains.len());
        
        // Reset is_running flag after cycle completes
        {
            let mut is_running = self.is_running.lock().await;
            *is_running = false;
        }
        
        Ok(total_discovered_wallets)
    }
    
    /// Execute discovery cycle for a specific chain
    async fn execute_discovery_cycle_for_chain(&self, chain: &str) -> Result<usize> {
        info!("🔄 Starting discovery cycle for chain: {}", chain);
        
        // Step 1: Get trending tokens using enhanced multi-sort discovery for this chain
        let trending_tokens = self.get_trending_tokens_for_chain(chain).await?;
        if trending_tokens.is_empty() {
            debug!("📊 No trending tokens found from multi-sort discovery");
            return Ok(0);
        }

        info!("📈 Paginated trending discovery: {} tokens (unlimited processing)", trending_tokens.len());
        
        // Safety mechanism: warn if processing a very large number of tokens
        if trending_tokens.len() > 1000 {
            warn!("⚠️ Processing {} trending tokens - this may take longer and use more API calls", trending_tokens.len());
        }

        let mut total_discovered_wallets = 0;

        // Step 2: For each trending token, get top traders
        for (i, token) in trending_tokens.iter().enumerate() {
            // Check if we should stop before processing each token
            {
                let is_running = self.is_running.lock().await;
                if !*is_running {
                    info!("🛑 Stop requested during token processing, breaking out of loop at token {}/{}", 
                          i + 1, trending_tokens.len());
                    break;
                }
            }

            debug!("🎯 Processing token {}/{}: {} ({})", 
                   i + 1, trending_tokens.len(), token.symbol, token.address);

            match self.get_top_traders_for_token(&token.address, chain).await {
                Ok(top_traders) => {
                    if !top_traders.is_empty() {
                        info!("👤 Found {} quality traders for {} ({})", 
                              top_traders.len(), token.symbol, token.address);

                        // Step 3: Push quality wallet-token pairs to Redis for P&L analysis
                        match self.push_wallet_token_pairs_to_queue(&top_traders, token, chain).await {
                            Ok(pushed_count) => {
                                total_discovered_wallets += pushed_count;
                                debug!("📤 Pushed {} wallets to analysis queue for {}", 
                                       pushed_count, token.symbol);
                            }
                            Err(e) => {
                                warn!("❌ Failed to push wallets for {}: {}", token.symbol, e);
                            }
                        }
                    } else {
                        debug!("⭕ No quality traders found for {} ({})", token.symbol, token.address);
                    }
                }
                Err(e) => {
                    warn!("❌ Failed to get top traders for {} ({}): {}", token.symbol, token.address, e);
                }
            }

            // Rate limiting between tokens (interruptible)
            if i < trending_tokens.len() - 1 {
                // Make this sleep interruptible by checking stop flag every 100ms
                let sleep_duration = Duration::from_millis(500);
                let check_interval = Duration::from_millis(100);
                let start_time = std::time::Instant::now();
                
                while start_time.elapsed() < sleep_duration {
                    tokio::time::sleep(check_interval).await;
                    
                    // Check if we should stop during rate limiting sleep
                    {
                        let is_running = self.is_running.lock().await;
                        if !*is_running {
                            info!("🛑 Stop requested during trending token rate limiting, breaking out early");
                            return Ok(total_discovered_wallets);
                        }
                    }
                }
            }
        }

        // Step 3: Get top gainers across different timeframes with pagination for this chain
        info!("🏆 Starting paginated multi-timeframe gainers discovery for chain: {}", chain);
        
        match self.get_top_gainers_for_chain(chain).await {
            Ok(gainers) => {
                if !gainers.is_empty() {
                    info!("💰 Found {} top gainers across all timeframes for chain {}", gainers.len(), chain);
                    
                    // Convert gainers to wallet-token pairs and push to queue
                    match self.push_gainers_to_queue(&gainers, "ALL_TIMEFRAMES", chain).await {
                        Ok(pushed_count) => {
                            total_discovered_wallets += pushed_count;
                            debug!("📤 Pushed {} gainer wallets to analysis queue for chain {}", pushed_count, chain);
                        }
                        Err(e) => {
                            warn!("❌ Failed to push gainers for chain {}: {}", chain, e);
                        }
                    }
                } else {
                    debug!("⭕ No gainers found across all timeframes for chain {}", chain);
                }
            }
            Err(e) => {
                warn!("❌ Failed to get gainers for chain {}: {}", chain, e);
            }
        }

        // Step 4: Get boosted tokens from DexScreener (NEW DISCOVERY SOURCE)
        if let Some(ref dexscreener_client) = self.dexscreener_client {
            info!("🚀 Starting DexScreener boosted token discovery");
            
            // Get both latest and top boosted tokens
            match dexscreener_client.get_all_boosted_tokens().await {
                Ok((latest_tokens, top_tokens)) => {
                    // Process latest boosted tokens
                    if !latest_tokens.is_empty() {
                        info!("📈 Found {} latest boosted tokens", latest_tokens.len());
                        match self.process_boosted_tokens(&latest_tokens, "latest").await {
                            Ok(pushed_count) => {
                                total_discovered_wallets += pushed_count;
                                debug!("📤 Pushed {} wallets from latest boosted tokens", pushed_count);
                            }
                            Err(e) => {
                                warn!("❌ Failed to process latest boosted tokens: {}", e);
                            }
                        }
                    }
                    
                    // Process top boosted tokens
                    if !top_tokens.is_empty() {
                        info!("🏆 Found {} top boosted tokens", top_tokens.len());
                        match self.process_boosted_tokens(&top_tokens, "top").await {
                            Ok(pushed_count) => {
                                total_discovered_wallets += pushed_count;
                                debug!("📤 Pushed {} wallets from top boosted tokens", pushed_count);
                            }
                            Err(e) => {
                                warn!("❌ Failed to process top boosted tokens: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("❌ Failed to fetch boosted tokens from DexScreener: {}", e);
                }
            }
        } else {
            debug!("⭕ DexScreener client disabled, skipping boosted token discovery");
        }

        // Step 5: Get newly listed tokens (NEW DISCOVERY SOURCE) for this chain
        if self.config.birdeye.new_listing_enabled {
            info!("🆕 Starting new listing token discovery for chain: {}", chain);
            
            match self.get_new_listing_tokens_for_chain(chain).await {
                Ok(new_listing_tokens) => {
                    if !new_listing_tokens.is_empty() {
                        info!("📈 Found {} new listing tokens for chain {}", new_listing_tokens.len(), chain);
                        
                        match self.process_new_listing_tokens(&new_listing_tokens).await {
                            Ok(pushed_count) => {
                                total_discovered_wallets += pushed_count;
                                debug!("📤 Pushed {} wallets from new listing tokens for chain {}", pushed_count, chain);
                            }
                            Err(e) => {
                                warn!("❌ Failed to process new listing tokens for chain {}: {}", chain, e);
                            }
                        }
                    } else {
                        debug!("⭕ No new listing tokens found for chain {}", chain);
                    }
                }
                Err(e) => {
                    warn!("❌ Failed to fetch new listing tokens for chain {}: {}", chain, e);
                }
            }
        } else {
            debug!("⭕ New listing token discovery disabled for chain {}", chain);
        }

        info!("✅ Enhanced Multi-Source Discovery Cycle Completed for chain {}: {} total quality wallets discovered", chain, total_discovered_wallets);
        debug!("📊 Discovery breakdown for chain {}: Paginated trending (unlimited tokens, 3 sorts × 5 pages = 15 calls) → paginated top traders (5x) | Paginated gainers (3 timeframes × 5 pages = 15 calls) → direct wallets | DexScreener boosted → paginated top traders (5x) | New listing tokens → paginated top traders (5x)", chain);
        Ok(total_discovered_wallets)
    }

    /// Get top gainers across all timeframes with pagination for a specific chain
    async fn get_top_gainers_for_chain(&self, chain: &str) -> Result<Vec<GainerLoser>> {
        debug!("💰 Fetching top gainers across all timeframes with pagination for chain: {}", chain);

        match self.birdeye_client.get_gainers_losers_paginated(chain).await {
            Ok(gainers) => {
                debug!("📊 Retrieved {} gainers across all timeframes and pages for chain {}", gainers.len(), chain);
                Ok(gainers)
            }
            Err(e) => {
                error!("❌ Failed to fetch gainers for chain {}: {}", chain, e);
                Err(e.into())
            }
        }
    }

    /// Push gainer wallets to Redis queue for P&L analysis
    async fn push_gainers_to_queue(&self, gainers: &[GainerLoser], timeframe: &str, chain: &str) -> Result<usize> {
        if gainers.is_empty() {
            return Ok(0);
        }

        // Convert gainers to DiscoveredWalletToken format
        // For gainers, we don't have a specific token, so we'll use a generic identifier
        let wallet_token_pairs: Vec<DiscoveredWalletToken> = gainers.iter()
            .map(|gainer| DiscoveredWalletToken {
                wallet_address: gainer.address.clone(),
                chain: chain.to_string(),
                token_address: "ALL_TOKENS".to_string(), // Generic for gainers
                token_symbol: format!("GAINER_{}", timeframe.to_uppercase()),
                trader_volume_usd: gainer.volume,
                trader_trades: gainer.trade_count,
                discovered_at: chrono::Utc::now(),
            })
            .collect();

        debug!("📤 Pushing {} gainer wallet-token pairs to Redis queue for timeframe {} on chain {}", 
               wallet_token_pairs.len(), timeframe, chain);

        let redis = self.redis_client.lock().await;
        if let Some(ref redis_client) = *redis {
            match redis_client.push_discovered_wallet_token_pairs_deduplicated(&wallet_token_pairs).await {
                Ok(pushed_count) => {
                    let skipped_count = wallet_token_pairs.len() - pushed_count;
                    if skipped_count > 0 {
                        info!("✅ Pushed {} new gainer wallet-token pairs for {} on chain {} (skipped {} duplicates)", 
                              pushed_count, timeframe, chain, skipped_count);
                    } else {
                        info!("✅ Successfully pushed {} gainer wallet-token pairs for {} on chain {}", 
                              pushed_count, timeframe, chain);
                    }
                    Ok(pushed_count)
                }
                Err(e) => {
                    error!("❌ Failed to push gainer wallet-token pairs to Redis queue: {}", e);
                    Err(e.into())
                }
            }
        } else {
            warn!("⚠️ Redis client not available, cannot push gainer wallet-token pairs");
            Ok(0)
        }
    }

    /// Process boosted tokens from DexScreener and get top traders for each
    async fn process_boosted_tokens(&self, boosted_tokens: &[DexScreenerBoostedToken], source: &str) -> Result<usize> {
        if boosted_tokens.is_empty() {
            return Ok(0);
        }

        debug!("🔄 Processing {} boosted tokens from {}", boosted_tokens.len(), source);
        
        // Filter for enabled chains only
        let mut processed_tokens: Vec<DexScreenerBoostedToken> = boosted_tokens
            .iter()
            .filter(|token| self.config.multichain.enabled_chains.contains(&token.chain_id))
            .cloned()
            .collect();
            
        // Limit to max boosted tokens (no filtering by boost amount needed)
        if processed_tokens.len() > self.config.dexscreener.max_boosted_tokens as usize {
            processed_tokens.truncate(self.config.dexscreener.max_boosted_tokens as usize);
        }

        debug!("📊 Processing {} boosted tokens from {}", processed_tokens.len(), source);

        let mut total_discovered_wallets = 0;

        // For each boosted token, get top traders using BirdEye
        for (i, boosted_token) in processed_tokens.iter().enumerate() {
            // Check if we should stop before processing each boosted token
            {
                let is_running = self.is_running.lock().await;
                if !*is_running {
                    info!("🛑 Stop requested during boosted token processing, breaking out of loop at token {}/{}", 
                          i + 1, processed_tokens.len());
                    break;
                }
            }

            debug!("🎯 Processing boosted token {}/{}: {}", 
                   i + 1, processed_tokens.len(), boosted_token.token_address);

            // Use the chain from the boosted token
            let boosted_chain = &boosted_token.chain_id;
            match self.get_top_traders_for_token(&boosted_token.token_address, boosted_chain).await {
                Ok(top_traders) => {
                    if !top_traders.is_empty() {
                        info!("👤 Found {} quality traders for boosted token {} ({})", 
                              top_traders.len(), boosted_token.token_address, source);

                        // Create a synthetic "trending token" structure for boosted tokens
                        let synthetic_token = BirdEyeTrendingToken {
                            address: boosted_token.token_address.clone(),
                            symbol: format!("BOOSTED_{}", source.to_uppercase()),
                            name: boosted_token.description.clone().unwrap_or_else(|| "Boosted Token".to_string()),
                            decimals: None,
                            price: 0.0, // Default price for boosted tokens
                            price_change_24h: None,
                            volume_24h: Some(1000.0), // Default volume for boosted tokens
                            volume_change_24h: None,
                            liquidity: None,
                            fdv: None,
                            marketcap: None,
                            rank: None,
                            logo_uri: None,
                            txns_24h: None,
                            last_trade_unix_time: None,
                        };

                        // Push quality wallet-token pairs to Redis for P&L analysis
                        match self.push_wallet_token_pairs_to_queue(&top_traders, &synthetic_token, boosted_chain).await {
                            Ok(pushed_count) => {
                                total_discovered_wallets += pushed_count;
                                debug!("📤 Pushed {} wallets to analysis queue for boosted token {}", 
                                       pushed_count, boosted_token.token_address);
                            }
                            Err(e) => {
                                warn!("❌ Failed to push wallets for boosted token {}: {}", 
                                      boosted_token.token_address, e);
                            }
                        }
                    } else {
                        debug!("⭕ No quality traders found for boosted token {}", boosted_token.token_address);
                    }
                }
                Err(e) => {
                    warn!("❌ Failed to get top traders for boosted token {}: {}", 
                          boosted_token.token_address, e);
                }
            }

            // Rate limiting between boosted tokens (interruptible)
            if i < processed_tokens.len() - 1 {
                // Make this sleep interruptible by checking stop flag every 100ms
                let sleep_duration = Duration::from_millis(500);
                let check_interval = Duration::from_millis(100);
                let start_time = std::time::Instant::now();
                
                while start_time.elapsed() < sleep_duration {
                    tokio::time::sleep(check_interval).await;
                    
                    // Check if we should stop during rate limiting sleep
                    {
                        let is_running = self.is_running.lock().await;
                        if !*is_running {
                            info!("🛑 Stop requested during boosted token rate limiting, breaking out early");
                            return Ok(total_discovered_wallets);
                        }
                    }
                }
            }
        }

        debug!("✅ Boosted token processing completed: {} total wallets discovered from {}", 
               total_discovered_wallets, source);
        Ok(total_discovered_wallets)
    }

    /// Get trending tokens for a specific chain using enhanced multi-sort discovery
    async fn get_trending_tokens_for_chain(&self, chain: &str) -> Result<Vec<BirdEyeTrendingToken>> {
        debug!("📊 Starting paginated trending token discovery from BirdEye for chain: {}", chain);

        match self.birdeye_client.get_trending_tokens_paginated(chain).await {
            Ok(mut tokens) => {
                info!("🎯 Paginated discovery completed: {} unique tokens found across all pages for chain {}", tokens.len(), chain);
                
                // Apply volume-based sorting (already done in multi-sort method but ensure consistency)
                tokens.sort_by(|a, b| b.volume_24h.partial_cmp(&a.volume_24h).unwrap_or(std::cmp::Ordering::Equal));
                
                // Apply max trending tokens limit (0 = unlimited)
                if self.config.birdeye.max_trending_tokens > 0 && tokens.len() > self.config.birdeye.max_trending_tokens {
                    tokens.truncate(self.config.birdeye.max_trending_tokens);
                    info!("📈 Processing trending tokens: {} tokens (limited to {}) for chain {}", tokens.len(), self.config.birdeye.max_trending_tokens, chain);
                } else {
                    info!("📈 Processing all discovered trending tokens: {} tokens for chain {}", tokens.len(), chain);
                }

                if self.config.system.debug_mode && !tokens.is_empty() {
                    debug!("🎯 Top trending tokens by multi-sort discovery for chain {}:", chain);
                    for (i, token) in tokens.iter().enumerate().take(8) {
                        debug!("  {}. {} ({}) - Vol: ${:.0}, Liq: ${:.0}, Change: {:.1}%", 
                               i + 1, token.symbol, token.address, 
                               token.volume_24h.unwrap_or(0.0),
                               token.liquidity.unwrap_or(0.0),
                               token.price_change_24h.unwrap_or(0.0));
                    }
                }

                Ok(tokens)
            }
            Err(e) => {
                error!("❌ Multi-sort trending token discovery failed for chain {}: {}", chain, e);
                warn!("🔄 Falling back to single-sort discovery method for chain {}", chain);
                
                // Fallback to original method
                match self.birdeye_client.get_trending_tokens_paginated(chain).await {
                    Ok(mut fallback_tokens) => {
                        fallback_tokens.sort_by(|a, b| b.volume_24h.partial_cmp(&a.volume_24h).unwrap_or(std::cmp::Ordering::Equal));
                        warn!("⚠️ Using fallback discovery: {} tokens retrieved for chain {}", fallback_tokens.len(), chain);
                        Ok(fallback_tokens)
                    }
                    Err(fallback_e) => {
                        error!("❌ Both multi-sort and fallback discovery failed for chain {}: {}", chain, fallback_e);
                        Err(e.into())
                    }
                }
            }
        }
    }

    /// Get top traders for a specific token on a specific chain
    async fn get_top_traders_for_token(&self, token_address: &str, chain: &str) -> Result<Vec<TopTrader>> {
        debug!("👥 Fetching top traders for token: {} on chain: {}", token_address, chain);

        match self.birdeye_client.get_top_traders_paginated(token_address, chain).await {
            Ok(traders) => {
                debug!("📊 Retrieved {} raw traders for token {} on chain {}", traders.len(), token_address, chain);

                // Apply quality filtering using trader filter config
                let quality_traders = self.birdeye_client.filter_top_traders(
                    traders,
                    self.config.trader_filter.min_capital_deployed_sol * 230.0, // Convert SOL to USD roughly
                    self.config.trader_filter.min_total_trades,
                    Some(self.config.trader_filter.min_win_rate),
                    Some(24), // Default to 24 hours
                );

                // Limit to max traders per token
                let mut filtered_traders = quality_traders;
                if filtered_traders.len() > self.config.birdeye.max_traders_per_token as usize {
                    filtered_traders.truncate(self.config.birdeye.max_traders_per_token as usize);
                }

                debug!("✅ Filtered to {} quality traders for token {} on chain {}", 
                       filtered_traders.len(), token_address, chain);

                if self.config.system.debug_mode && !filtered_traders.is_empty() {
                    for (i, trader) in filtered_traders.iter().enumerate().take(3) {
                        debug!("  {}. {} - Volume: ${:.0}, Trades: {}", 
                               i + 1, trader.owner, trader.volume, trader.trade);
                    }
                }

                Ok(filtered_traders)
            }
            Err(e) => {
                warn!("❌ Failed to fetch top traders for token {} on chain {}: {}", token_address, chain, e);
                Err(e.into())
            }
        }
    }

    /// Push quality wallet-token pairs to Redis queue for targeted P&L analysis
    async fn push_wallet_token_pairs_to_queue(&self, traders: &[TopTrader], token: &BirdEyeTrendingToken, chain: &str) -> Result<usize> {
        if traders.is_empty() {
            return Ok(0);
        }

        let wallet_token_pairs: Vec<DiscoveredWalletToken> = traders.iter()
            .map(|trader| DiscoveredWalletToken {
                wallet_address: trader.owner.clone(),
                chain: chain.to_string(),
                token_address: token.address.clone(),
                token_symbol: token.symbol.clone(),
                trader_volume_usd: trader.volume,
                trader_trades: trader.trade,
                discovered_at: chrono::Utc::now(),
            })
            .collect();

        debug!("📤 Pushing {} wallet-token pairs to Redis queue for token {} on chain {}", wallet_token_pairs.len(), token.symbol, chain);

        let redis = self.redis_client.lock().await;
        if let Some(ref redis_client) = *redis {
            match redis_client.push_discovered_wallet_token_pairs_deduplicated(&wallet_token_pairs).await {
                Ok(pushed_count) => {
                    let skipped_count = wallet_token_pairs.len() - pushed_count;
                    if skipped_count > 0 {
                        info!("✅ Pushed {} new wallet-token pairs to analysis queue for {} on chain {} (skipped {} duplicates)", 
                              pushed_count, token.symbol, chain, skipped_count);
                    } else {
                        info!("✅ Successfully pushed {} quality wallet-token pairs to analysis queue for {} on chain {}", 
                              pushed_count, token.symbol, chain);
                    }
                    Ok(pushed_count)
                }
                Err(e) => {
                    error!("❌ Failed to push wallet-token pairs to Redis queue: {}", e);
                    Err(e.into())
                }
            }
        } else {
            warn!("⚠️ Redis client not available, cannot push wallet-token pairs");
            Ok(0)
        }
    }

    // get_wallet_transaction_history method removed - was unused and relied on removed get_trader_transactions

    /// Get new listing tokens with comprehensive coverage for a specific chain
    async fn get_new_listing_tokens_for_chain(&self, chain: &str) -> Result<Vec<NewListingToken>> {
        debug!("🆕 Fetching new listing tokens with comprehensive coverage for chain: {}", chain);
        
        let all_tokens = self.birdeye_client.get_new_listing_tokens_comprehensive(chain).await?;
        
        // Apply quality filtering
        let filter = NewListingTokenFilter {
            min_liquidity: Some(self.config.birdeye.new_listing_min_liquidity),
            max_age_hours: Some(self.config.birdeye.new_listing_max_age_hours),
            max_tokens: Some(self.config.birdeye.new_listing_max_tokens),
            exclude_sources: None,
        };
        
        let filtered_tokens = self.birdeye_client.filter_new_listing_tokens(all_tokens, &filter);
        
        info!("🎯 New listing discovery completed: {} quality tokens after filtering for chain {}", filtered_tokens.len(), chain);
        
        if self.config.system.debug_mode && !filtered_tokens.is_empty() {
            debug!("🆕 Top new listing tokens for chain {}:", chain);
            for (i, token) in filtered_tokens.iter().enumerate().take(8) {
                debug!("  {}. {} ({}) - Liquidity: ${:.2}, Source: {}", 
                       i + 1, token.symbol, token.address, token.liquidity, token.source);
            }
        }
        
        Ok(filtered_tokens)
    }


    /// Process new listing tokens and get top traders for each
    async fn process_new_listing_tokens(&self, new_listing_tokens: &[NewListingToken]) -> Result<usize> {
        if new_listing_tokens.is_empty() {
            return Ok(0);
        }
        
        debug!("🔄 Processing {} new listing tokens", new_listing_tokens.len());
        
        let mut total_discovered_wallets = 0;
        
        for (i, token) in new_listing_tokens.iter().enumerate() {
            // Check if we should stop before processing each new listing token
            {
                let is_running = self.is_running.lock().await;
                if !*is_running {
                    info!("🛑 Stop requested during new listing token processing, breaking out of loop at token {}/{}", 
                          i + 1, new_listing_tokens.len());
                    break;
                }
            }

            debug!("🎯 Processing new listing token {}/{}: {} ({})", 
                   i + 1, new_listing_tokens.len(), token.symbol, token.address);
            
            // Use default chain for new listing tokens
            let listing_chain = &self.config.multichain.default_chain;
            match self.get_top_traders_for_token(&token.address, listing_chain).await {
                Ok(top_traders) => {
                    if !top_traders.is_empty() {
                        info!("👤 Found {} quality traders for new listing token {} ({})", 
                              top_traders.len(), token.symbol, token.address);
                        
                        // Convert NewListingToken to TrendingToken format for compatibility
                        let synthetic_trending_token = BirdEyeTrendingToken {
                            address: token.address.clone(),
                            symbol: token.symbol.clone(),
                            name: token.name.clone(),
                            decimals: Some(token.decimals),
                            price: 0.0, // Will be fetched by price service
                            price_change_24h: None,
                            volume_24h: Some(token.liquidity), // Use liquidity as volume proxy
                            volume_change_24h: None,
                            liquidity: Some(token.liquidity),
                            fdv: None,
                            marketcap: None,
                            rank: None,
                            logo_uri: token.logo_uri.clone(),
                            txns_24h: None,
                            last_trade_unix_time: None,
                        };
                        
                        // Use existing wallet-token pair pushing logic
                        match self.push_wallet_token_pairs_to_queue(&top_traders, &synthetic_trending_token, listing_chain).await {
                            Ok(pushed_count) => {
                                total_discovered_wallets += pushed_count;
                                debug!("📤 Pushed {} wallets for new listing token {}", pushed_count, token.symbol);
                            }
                            Err(e) => {
                                warn!("❌ Failed to push wallets for new listing token {}: {}", token.symbol, e);
                            }
                        }
                    } else {
                        debug!("⭕ No quality traders found for new listing token {}", token.symbol);
                    }
                }
                Err(e) => {
                    warn!("❌ Failed to get top traders for new listing token {}: {}", token.symbol, e);
                }
            }
            
            // Rate limiting between tokens (interruptible)
            if i < new_listing_tokens.len() - 1 {
                // Make this sleep interruptible by checking stop flag every 100ms
                let sleep_duration = Duration::from_millis(500);
                let check_interval = Duration::from_millis(100);
                let start_time = std::time::Instant::now();
                
                while start_time.elapsed() < sleep_duration {
                    tokio::time::sleep(check_interval).await;
                    
                    // Check if we should stop during rate limiting sleep
                    {
                        let is_running = self.is_running.lock().await;
                        if !*is_running {
                            info!("🛑 Stop requested during new listing token rate limiting, breaking out early");
                            return Ok(total_discovered_wallets);
                        }
                    }
                }
            }
        }
        
        info!("✅ New listing token processing completed: {} total wallets discovered", total_discovered_wallets);
        Ok(total_discovered_wallets)
    }

    /// Get statistics about the current discovery state
    pub async fn get_discovery_stats(&self) -> Result<DiscoveryStats> {
        let redis = self.redis_client.lock().await;
        if let Some(ref redis_client) = *redis {
            let queue_size = redis_client.get_wallet_queue_size().await.unwrap_or(0);
            
            Ok(DiscoveryStats {
                is_running: *self.is_running.lock().await,
                wallet_queue_size: queue_size as u32,
                config: self.config.clone(),
                tokens_discovered: 0, // TODO: Track this metric
                wallet_token_pairs_discovered: queue_size as u32,
                new_listing_tokens_discovered: 0, // TODO: Track this metric
                new_listing_wallets_discovered: 0, // TODO: Track this metric
            })
        } else {
            Ok(DiscoveryStats {
                is_running: *self.is_running.lock().await,
                wallet_queue_size: 0,
                config: self.config.clone(),
                tokens_discovered: 0,
                wallet_token_pairs_discovered: 0,
                new_listing_tokens_discovered: 0,
                new_listing_wallets_discovered: 0,
            })
        }
    }
}

/// Statistics about the discovery process
#[derive(Debug, Clone)]
pub struct DiscoveryStats {
    pub is_running: bool,
    pub wallet_queue_size: u32,
    pub config: SystemConfig,
    pub tokens_discovered: u32,
    pub wallet_token_pairs_discovered: u32,
    pub new_listing_tokens_discovered: u32,
    pub new_listing_wallets_discovered: u32,
}

/// Processed swap transaction for BirdEye data analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessedSwap {
    pub token_in: String,
    pub token_out: String,
    pub amount_in: Decimal,
    pub amount_out: Decimal,
    pub sol_equivalent: Decimal,
    pub price_per_token: Decimal,
    pub tx_hash: String,
    pub timestamp: i64,
    pub source: String,
}

impl ProcessedSwap {
    /// Process BirdEye transactions into ProcessedSwap format
    pub fn from_birdeye_transactions(transactions: &[GeneralTraderTransaction]) -> Result<Vec<ProcessedSwap>> {
        let mut processed_swaps = Vec::new();
        
        for tx in transactions {
            // Determine which token is being sold (from) and which is being bought (to)
            let (token_in, amount_in, token_out, amount_out) = if tx.quote.type_swap == "from" {
                // Quote token is being sold, base token is being bought
                (
                    tx.quote.address.clone(),
                    Decimal::from_f64_retain(tx.quote.ui_amount).unwrap_or_default(),
                    tx.base.address.clone(),
                    Decimal::from_f64_retain(tx.base.ui_amount).unwrap_or_default(),
                )
            } else {
                // Base token is being sold, quote token is being bought  
                (
                    tx.base.address.clone(),
                    Decimal::from_f64_retain(tx.base.ui_amount).unwrap_or_default(),
                    tx.quote.address.clone(),
                    Decimal::from_f64_retain(tx.quote.ui_amount).unwrap_or_default(),
                )
            };
            
            // Calculate SOL equivalent and price
            let sol_mint = "So11111111111111111111111111111111111111112";
            let sol_equivalent = if token_in == sol_mint {
                amount_in
            } else if token_out == sol_mint {
                amount_out
            } else {
                // Use quote price to estimate SOL equivalent
                Decimal::from_f64_retain(tx.quote_price).unwrap_or_default() * amount_in
            };
            
            let price_per_token = if token_out == sol_mint {
                // Selling token for SOL
                if amount_out > Decimal::ZERO {
                    amount_out / amount_in
                } else {
                    Decimal::ZERO
                }
            } else if token_in == sol_mint {
                // Buying token with SOL
                tx.base.price.map(|p| Decimal::from_f64_retain(p).unwrap_or_default())
                    .unwrap_or_else(|| {
                        if amount_in > Decimal::ZERO {
                            amount_out / amount_in
                        } else {
                            Decimal::ZERO
                        }
                    })
            } else {
                // Token to token swap - use base price
                tx.base.price.map(|p| Decimal::from_f64_retain(p).unwrap_or_default())
                    .unwrap_or_default()
            };
            
            processed_swaps.push(ProcessedSwap {
                token_in,
                token_out,
                amount_in,
                amount_out,
                sol_equivalent,
                price_per_token,
                tx_hash: tx.tx_hash.clone(),
                timestamp: tx.block_unix_time,
                source: tx.source.clone(),
            });
        }
        
        Ok(processed_swaps)
    }
    
    // LEGACY METHOD REMOVED: to_financial_event()
    // This method converted ProcessedSwap to legacy FinancialEvent format
    // New P&L engine uses GeneralTraderTransaction directly with embedded prices
}

// Tests removed - will use integration tests with SystemConfig