#[cfg(test)]
#[path = "../../../tests/unit/format/solution/initial_reader_test.rs"]
mod initial_reader_test;

use crate::format::problem::JobIndex;
use crate::format::solution::deserialize_solution;
use crate::format::CoordIndex;
use crate::parse_time;
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read};
use std::iter::once;
use std::sync::Arc;
use vrp_core::models::common::*;
use vrp_core::models::problem::{Actor, Job, Single};
use vrp_core::models::solution::{Activity, Place, Registry, Route};
use vrp_core::models::{Problem, Solution};

use crate::format::solution::Activity as FormatActivity;
use crate::format::solution::Stop as FormatStop;
use crate::format::solution::Tour as FormatTour;

use vrp_core::models::solution::Activity as CoreActivity;
use vrp_core::models::solution::Tour as CoreTour;

type ActorKey = (String, String, usize);

/// Reads initial solution from buffer.
/// NOTE: Solution feasibility is not checked.
pub fn read_init_solution<R: Read>(solution: BufReader<R>, problem: Arc<Problem>) -> Result<Solution, String> {
    let solution = deserialize_solution(solution).map_err(|err| format!("cannot deserialize solution: {}", err))?;

    let mut registry = Registry::new(&problem.fleet);
    let actor_index = registry.all().map(|actor| (get_actor_key(actor.as_ref()), actor)).collect::<HashMap<_, _>>();
    let coord_index = get_coord_index(problem.as_ref());
    let job_index = get_job_index(problem.as_ref());

    let routes = solution.tours.iter().try_fold::<_, _, Result<_, String>>(Default::default(), |routes, tour| {
        let actor_key = (tour.vehicle_id.clone(), tour.type_id.clone(), tour.shift_index);
        let actor =
            actor_index.get(&actor_key).ok_or_else(|| format!("cannot find vehicle for {:?}", actor_key))?.clone();
        registry.use_actor(&actor);

        let mut core_route = create_core_route(actor);

        tour.stops.iter().try_for_each(|stop| {
            stop.activities.iter().try_for_each::<_, Result<_, String>>(|activity| {
                try_insert_activity(&mut core_route, tour, stop, activity, job_index, coord_index)
            })
        })?;

        Ok(routes)
    })?;

    let unassigned = solution.unassigned.unwrap_or_default().iter().try_fold::<Vec<_>, _, Result<_, String>>(
        Default::default(),
        |mut acc, unassigned_job| {
            let job = job_index
                .get(&unassigned_job.job_id)
                .cloned()
                .ok_or_else(|| format!("cannot get job id for: {:?}", unassigned_job))?;
            let code = unassigned_job
                .reasons
                .first()
                .map(|reason| reason.code)
                .ok_or_else(|| format!("cannot get reason for: {:?}", unassigned_job))?;

            acc.push((job, code));

            Ok(acc)
        },
    )?;

    Ok(Solution { registry, routes, unassigned, extras: problem.extras.clone() })
}

fn get_job_index(problem: &Problem) -> &JobIndex {
    problem
        .extras
        .get("job_index")
        .and_then(|s| s.downcast_ref::<JobIndex>())
        .unwrap_or_else(|| panic!("cannot get job index!"))
}

fn get_coord_index(problem: &Problem) -> &CoordIndex {
    problem
        .extras
        .get("coord_index")
        .and_then(|s| s.downcast_ref::<CoordIndex>())
        .unwrap_or_else(|| panic!("Cannot get coord index!"))
}

fn get_actor_key(actor: &Actor) -> ActorKey {
    let dimens = &actor.vehicle.dimens;

    let vehicle_id = dimens.get_id().cloned().expect("cannot get vehicle id!");
    let type_id = dimens.get_value::<String>("type_id").cloned().expect("cannot get type id!");
    let shift_index = dimens.get_value::<usize>("shift_index").cloned().expect("cannot get shift index!");

    (vehicle_id, type_id, shift_index)
}

fn create_core_route(actor: Arc<Actor>) -> Route {
    let tour = CoreTour::new(&actor);
    Route { actor, tour }
}

fn try_insert_activity(
    route: &mut Route,
    tour: &FormatTour,
    stop: &FormatStop,
    activity: &FormatActivity,
    job_index: &JobIndex,
    coord_index: &CoordIndex,
) -> Result<(), String> {
    let act_type = activity.activity_type.as_str();
    let act_tag = activity.job_tag.as_ref();
    let act_time = activity
        .time
        .as_ref()
        .map(|time| TimeWindow::new(parse_time(&time.start), parse_time(&time.end)))
        .unwrap_or_else(|| TimeWindow::new(parse_time(&stop.time.arrival), parse_time(&stop.time.departure)));
    let act_location = coord_index
        .get_by_loc(activity.location.as_ref().unwrap_or(&stop.location))
        .ok_or_else(|| format!("cannot get location for activity for job '{}'", activity.job_id))?;
    let route_start_time =
        tour.stops.first().map(|stop| parse_time(&stop.time.departure)).ok_or_else(|| "empty route".to_owned())?;

    match act_type {
        "departure" | "arrival" => Ok(()),
        "pickup" | "delivery" | "replacement" | "service" => {
            let job =
                job_index.get(&activity.job_id).ok_or_else(|| format!("unknown job id: '{}'", activity.job_id))?;
            let singles: Box<dyn Iterator<Item = &Arc<_>>> = match job {
                Job::Single(single) => Box::new(once(single)),
                Job::Multi(multi) => {
                    let tags = multi.jobs.iter().filter_map(|job| get_tag(job).cloned()).collect::<HashSet<_>>();
                    if tags.len() < multi.jobs.len() {
                        return Err(format!(
                            "initial solution requires multi job to have unique tags, check '{}' job",
                            activity.job_id
                        ));
                    }

                    Box::new(multi.jobs.iter())
                }
            };
            let single = singles
                .filter(|single| {
                    is_correct_single(
                        single,
                        route_start_time,
                        true,
                        act_location,
                        &act_time,
                        &activity.job_id,
                        act_tag,
                    )
                })
                .next()
                .ok_or_else(|| format!("cannot match job '{}'", activity.job_id))?;

            try_insert_single(route, single, act_location)
        }
        "break" | "depot" | "reload" => {
            let single = (1..)
                .map(|idx| format!("{}_{}_{}_{}", tour.vehicle_id, act_type, tour.shift_index, idx))
                .map(|job_id| job_index.get(&job_id))
                .take_while(|job| job.is_some())
                .filter_map(|job| job.and_then(|job| job.as_single()))
                .filter(|single| {
                    is_correct_single(
                        single,
                        route_start_time,
                        false,
                        act_location,
                        &act_time,
                        &activity.job_id,
                        act_tag,
                    )
                })
                .next()
                .ok_or_else(|| format!("cannot match '{}' for '{}'", act_type, tour.vehicle_id))?;

            try_insert_single(route, single, act_location)
        }
        _ => Err(format!("unknown activity type: {}", activity.activity_type)),
    }
}

fn get_tag(single: &Single) -> Option<&String> {
    single.dimens.get_value::<String>("tag")
}

fn is_correct_single(
    single: &Arc<Single>,
    route_start_time: Timestamp,
    is_job_activity: bool,
    act_location: Location,
    act_time: &TimeWindow,
    act_job_id: &String,
    act_tag: Option<&String>,
) -> bool {
    let job_id = create_activity(single.clone(), act_location)
        .retrieve_job()
        .unwrap()
        .dimens()
        .get_id()
        .cloned()
        .expect("cannot get job id");

    let is_same_ids = *act_job_id == job_id;
    let is_same_tags = match (get_tag(single), act_tag) {
        (Some(job_tag), Some(activity_tag)) => job_tag == activity_tag,
        (None, None) => true,
        _ => false,
    };

    match (is_same_tags, is_same_ids, is_job_activity) {
        (true, true, _) => true,
        (true, false, true) => false,
        (true, false, false) => single.places.iter().any(|place| {
            let is_same_location = place.location.map_or(true, |l| l == act_location);
            let is_proper_time = place.times.iter().any(|time| time.intersects(route_start_time, act_time));

            is_same_location && is_proper_time
        }),
        _ => false,
    }
}

fn try_insert_single(route: &mut Route, single: &Arc<Single>, location: Location) -> Result<(), String> {
    let activity = create_activity(single.clone(), location);
    route.tour.insert_last(activity);

    Ok(())
}

fn create_activity(single: Arc<Single>, location: Location) -> CoreActivity {
    Activity {
        place: Place { location, duration: 0.0, time: TimeWindow::new(0., 0.) },
        schedule: Schedule { arrival: 0.0, departure: 0.0 },
        job: Some(single),
    }
}
