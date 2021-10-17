// SPDX-FileCopyrightText: 2021 Softbear, Inc.
// SPDX-License-Identifier: AGPL-3.0-or-later

use arrayvec::ArrayVec;
use common::altitude::Altitude;
use common::angle::Angle;
use common::complete::CompleteTrait;
use common::contact::ContactTrait;
use common::entity::*;
use common::guidance::Guidance;
use common::protocol::*;
use common::terrain;
use common::terrain::Terrain;
use common::ticks::Ticks;
use common::util::gen_radius;
use glam::Vec2;
use rand::seq::IteratorRandom;
use rand::{thread_rng, Rng};

/// Bot implements a ship-controlling AI that is, in many ways, equivalent to a player.
pub struct Bot {
    /// Bot's chance of attacking, randomized to improve variety of bots.
    aggression: f32,
    /// Amount to offset aiming by. This creates more interesting hit patterns.
    aim_bias: Vec2,
    /// Maximum level bot will try to upgrade to, randomized to improve variety of bots.
    level_ambition: u8,
    /// Whether the bot spawned at least once, and therefore is capable of rage-quitting.
    spawned_at_least_once: bool,
}

impl Bot {
    /// This arbitrary value controls how chill the bots are. If too high, bots are trigger-happy
    /// maniacs, and the waters get filled with stray torpedoes.
    const MAX_AGGRESSION: f32 = 0.1;

    pub fn new() -> Self {
        let mut rng = thread_rng();
        Self {
            // Raise aggression to a power such that lower values are more common.
            aggression: rng.gen::<f32>().powi(2) * Self::MAX_AGGRESSION,
            aim_bias: gen_radius(&mut rng, 10.0),
            level_ambition: rng.gen_range(1..EntityData::MAX_BOAT_LEVEL),
            spawned_at_least_once: false,
        }
    }

    /// update processes a complete update and returns some commands to execute, and a boolean
    /// of whether to quit.
    pub fn update<'a, U: 'a + CompleteTrait<'a>>(
        &mut self,
        mut update: U,
    ) -> (ArrayVec<Command, 2>, bool) {
        let mut ret = ArrayVec::new();
        let mut quit = false;
        let mut rng = thread_rng();

        let player_id = update.player_id();
        let mut contacts = update.contacts();
        let terrain = update.terrain();

        if let Some(boat) = contacts
            .next()
            .filter(|c| c.is_boat() && c.player_id() == Some(player_id))
        {
            self.spawned_at_least_once = true;

            let boat_type: EntityType = boat.entity_type().unwrap();
            let data: &EntityData = boat_type.data();
            let health_percent = 1.0 - boat.damage().to_secs() / data.max_health().to_secs();

            // Weighted sums of direction vectors for various purposes.
            let mut movement = Vec2::ZERO;

            let attract = |weighted_sum: &mut Vec2, target_delta: Vec2, distance_squared: f32| {
                *weighted_sum += target_delta / (1.0 + distance_squared);
            };

            let repel = |weighted_sum: &mut Vec2, target_delta: Vec2, distance_squared: f32| {
                attract(weighted_sum, -target_delta, distance_squared);
            };

            let spring = |weighted_sum: &mut Vec2, target_delta: Vec2, desired_distance: f32| {
                let distance = target_delta.length();
                let displacement = distance - desired_distance;
                *weighted_sum = target_delta * displacement / (displacement.powi(2) + 1.0);
            };

            // Terrain.
            const SAMPLES: u32 = 10;
            for i in 0..SAMPLES {
                let angle =
                    Angle::from_radians(i as f32 * (2.0 * std::f32::consts::PI / SAMPLES as f32));
                let delta_position = angle.to_vec() * data.length;
                if Self::is_land_or_border(
                    boat.transform().position + delta_position,
                    terrain,
                    update.world_radius(),
                ) {
                    repel(&mut movement, delta_position, data.length.powi(2));
                }
            }

            let mut closest_enemy: Option<(U::Contact, f32)> = None;

            // Scan sensor contacts to help make decisions.
            for contact in contacts {
                if contact.id() == boat.id() {
                    // Skip processing self.
                    continue;
                }

                if let Some(contact_data) = contact.entity_type().map(EntityType::data) {
                    let delta_position = contact.transform().position - boat.transform().position;
                    let distance_squared = delta_position.length_squared();

                    let friendly = contact.player_id() == Some(player_id);

                    if contact_data.kind == EntityKind::Collectible {
                        attract(&mut movement, delta_position, distance_squared);
                    } else if (!friendly || contact_data.kind == EntityKind::Boat)
                        && !(!friendly
                            && contact_data.kind == EntityKind::Boat
                            && data.sub_kind == EntitySubKind::Ram)
                    {
                        repel(&mut movement, delta_position, distance_squared);
                    }

                    if friendly {
                        if contact_data.kind == EntityKind::Boat {
                            spring(
                                &mut movement,
                                delta_position,
                                data.radius + contact_data.radius,
                            );
                        }
                    } else {
                        if match contact_data.kind {
                            EntityKind::Boat | EntityKind::Aircraft => true,
                            EntityKind::Weapon => contact_data.sub_kind == EntitySubKind::Missile,
                            EntityKind::Obstacle => {
                                repel(&mut movement, delta_position, distance_squared);
                                false
                            }
                            _ => false,
                        } {
                            if let Some(existing) = &closest_enemy {
                                if distance_squared < existing.1 {
                                    closest_enemy = Some((contact, distance_squared));
                                }
                            } else {
                                closest_enemy = Some((contact, distance_squared));
                            }
                        }
                    }
                }
            }

            let mut best_firing_solution = None;

            if let Some((enemy, _)) = closest_enemy {
                let reloads = boat.reloads();
                let enemy_data = enemy.data();
                for (i, armament) in data.armaments.iter().enumerate() {
                    if reloads[i] > Ticks::ZERO {
                        // Not yet reloaded.
                        continue;
                    }

                    let armament_entity_data: &EntityData = armament.entity_type.data();
                    match armament_entity_data.kind {
                        EntityKind::Weapon | EntityKind::Aircraft => {}
                        _ => continue,
                    }

                    let relevant = match enemy_data.kind {
                        EntityKind::Aircraft | EntityKind::Weapon => {
                            if enemy.altitude().is_airborne() {
                                matches!(armament_entity_data.sub_kind, EntitySubKind::Sam)
                            } else {
                                false
                            }
                        }
                        EntityKind::Boat => {
                            if enemy.altitude().is_submerged() {
                                matches!(
                                    armament_entity_data.sub_kind,
                                    EntitySubKind::Torpedo
                                        | EntitySubKind::Plane
                                        | EntitySubKind::Heli
                                        | EntitySubKind::DepthCharge
                                )
                            } else {
                                matches!(
                                    armament_entity_data.sub_kind,
                                    EntitySubKind::Torpedo
                                        | EntitySubKind::Plane
                                        | EntitySubKind::Heli
                                        | EntitySubKind::DepthCharge
                                        | EntitySubKind::Rocket
                                        | EntitySubKind::Missile
                                        | EntitySubKind::Shell
                                )
                            }
                        }
                        _ => false,
                    };

                    if !relevant {
                        continue;
                    }

                    if let Some(turret_index) = armament.turret {
                        if !data.turrets[turret_index].within_azimuth(boat.turrets()[turret_index])
                        {
                            // Out of azimuth range; cannot fire.
                            continue;
                        }
                    }

                    let transform = *boat.transform() + data.armament_transform(boat.turrets(), i);
                    let angle = Angle::from(enemy.transform().position - transform.position);

                    let mut angle_diff = (angle - transform.direction).abs();
                    if armament.vertical || armament_entity_data.kind == EntityKind::Aircraft {
                        angle_diff = Angle::ZERO;
                    }

                    let firing_solution = (i as u8, enemy.transform().position, angle_diff);

                    if firing_solution.2
                        < best_firing_solution
                            .map(|s: (u8, Vec2, Angle)| s.2)
                            .unwrap_or(Angle::MAX)
                    {
                        best_firing_solution = Some(firing_solution);
                    }
                }
            }

            ret.push(Command::Control(Control {
                guidance: Some(Guidance {
                    direction_target: Angle::from(movement),
                    velocity_target: data.speed * 0.8,
                }),
                angular_velocity_target: None,
                altitude_target: if data.sub_kind == EntitySubKind::Submarine {
                    Some(if health_percent > self.aggression {
                        Altitude::ZERO
                    } else {
                        Altitude::MIN
                    })
                } else {
                    None
                },
                aim_target: best_firing_solution.map(|solution| solution.1 + self.aim_bias),
                active: health_percent >= 0.5,
            }));

            if rng.gen_bool(self.aggression as f64) {
                if best_firing_solution.is_some() {
                    let firing_solution = best_firing_solution.unwrap();
                    if firing_solution.2 < Angle::from_degrees(60.0) {
                        ret.push(Command::Fire(Fire {
                            index: firing_solution.0,
                            position_target: firing_solution.1,
                        }));
                    }
                } else if data.level < self.level_ambition {
                    // Upgrade, if possible.
                    if let Some(entity_type) = boat_type
                        .upgrade_options(update.score(), true)
                        .choose(&mut rng)
                    {
                        ret.push(Command::Upgrade(Upgrade { entity_type }))
                    }
                }
            }
        } else if self.spawned_at_least_once && rng.gen_bool(1.0 / 3.0) {
            // Rage quit.
            quit = true;
        } else {
            ret.push(Command::Spawn(Spawn {
                entity_type: EntityType::spawn_options(true)
                    .choose(&mut rng)
                    .expect("there must be at least one entity type to spawn as"),
            }));
        }

        (ret, quit)
    }

    /// Returns true if there is land or border at the given position.
    fn is_land_or_border(pos: Vec2, terrain: &Terrain, world_radius: f32) -> bool {
        if pos.length_squared() > world_radius.powi(2) {
            return true;
        }

        terrain.sample(pos).unwrap_or(Altitude::MIN) >= terrain::SAND_LEVEL
    }
}