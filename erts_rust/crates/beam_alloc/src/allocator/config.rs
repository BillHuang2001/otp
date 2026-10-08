// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! Allocator configuration: [`AllocatorConfig`] and the [`AllocatorConfigBuilder`]
//! that fills it in.

use crate::policy::CarrierPolicy;
use crate::size_class::SizeClasses;
use crate::strategy::Strategy;

/// Configuration for a [`CarrierAllocator`](super::CarrierAllocator).
///
/// Build one with [`AllocatorConfig::builder`] or take the defaults via
/// [`AllocatorConfig::default`].
#[derive(Clone, Debug)]
pub struct AllocatorConfig {
    pub(super) name: String,
    strategy: Strategy,
    pub(super) policy: CarrierPolicy,
    pub(super) carrier_size: usize,
    pub(super) classes: SizeClasses,
    pub(super) max_footprint: Option<usize>,
    zeroed_carriers: bool,
}
impl AllocatorConfig {
    /// The default carrier size (1 MiB).
    pub const DEFAULT_CARRIER_SIZE: usize = 1024 * 1024;

    /// Starts building a configuration from the defaults.
    pub fn builder() -> AllocatorConfigBuilder {
        AllocatorConfigBuilder {
            config: AllocatorConfig::default(),
        }
    }

    /// The allocator name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The selected allocation strategy.
    pub fn strategy(&self) -> Strategy {
        self.strategy
    }

    /// The legacy carrier policy this allocator routes and sizes carriers by
    /// (`+M<alloc><opt>`: `sbct`, `smbcs`/`lmbcs`, `mmmbc`, …).
    pub fn policy(&self) -> &CarrierPolicy {
        &self.policy
    }
    /// The carrier size in bytes.
    pub fn carrier_size(&self) -> usize {
        self.carrier_size
    }

    /// The size-class table.
    pub fn classes(&self) -> &SizeClasses {
        &self.classes
    }

    /// The optional total-footprint cap in bytes (used by tests).
    pub fn max_footprint(&self) -> Option<usize> {
        self.max_footprint
    }

    /// Whether freshly mapped carriers are handed out zeroed.
    pub fn zeroed_carriers(&self) -> bool {
        self.zeroed_carriers
    }
}

impl Default for AllocatorConfig {
    fn default() -> AllocatorConfig {
        AllocatorConfig {
            name: String::from("beam_alloc"),
            strategy: Strategy::FirstFit,
            policy: CarrierPolicy::default(),
            carrier_size: AllocatorConfig::DEFAULT_CARRIER_SIZE,
            classes: SizeClasses::default_small_block(),
            max_footprint: None,
            zeroed_carriers: true,
        }
    }
}
/// Builder for [`AllocatorConfig`]; obtain it from [`AllocatorConfig::builder`].
#[derive(Clone, Debug)]
pub struct AllocatorConfigBuilder {
    config: AllocatorConfig,
}

impl AllocatorConfigBuilder {
    /// Sets the allocator name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.config.name = name.into();
        self
    }

    /// Sets the allocation strategy.
    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.config.strategy = strategy;
        self
    }

    /// Sets the legacy carrier policy (`+M<alloc><opt>` carrier thresholds/limits)
    /// that decides which requests share a carrier and how big a fresh one is.
    pub fn policy(mut self, policy: CarrierPolicy) -> Self {
        self.config.policy = policy;
        self
    }
    /// Sets the carrier size in bytes.
    pub fn carrier_size(mut self, bytes: usize) -> Self {
        self.config.carrier_size = bytes;
        self
    }

    /// Sets the size-class table.
    pub fn classes(mut self, classes: SizeClasses) -> Self {
        self.config.classes = classes;
        self
    }

    /// Sets (or clears) the total-footprint cap in bytes.
    pub fn max_footprint(mut self, bytes: Option<usize>) -> Self {
        self.config.max_footprint = bytes;
        self
    }

    /// Sets whether freshly mapped carriers are handed out zeroed.
    pub fn zeroed_carriers(mut self, zeroed: bool) -> Self {
        self.config.zeroed_carriers = zeroed;
        self
    }

    /// Finishes the configuration.
    pub fn build(self) -> AllocatorConfig {
        self.config
    }
}
