use super::*;

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum GoalStatus {
    Unknown,
    Accepted,
    Executing,
    Canceling,
    Succeeded,
    Canceled,
    Aborted,
}

impl GoalStatus {
    #[allow(dead_code)]
    pub fn to_rcl(&self) -> i8 {
        match self {
            GoalStatus::Unknown => 0,
            GoalStatus::Accepted => 1,
            GoalStatus::Executing => 2,
            GoalStatus::Canceling => 3,
            GoalStatus::Succeeded => 4,
            GoalStatus::Canceled => 5,
            GoalStatus::Aborted => 6,
        }
    }

    pub fn from_rcl(s: i8) -> Self {
        match s {
            0 => GoalStatus::Unknown,
            1 => GoalStatus::Accepted,
            2 => GoalStatus::Executing,
            3 => GoalStatus::Canceling,
            4 => GoalStatus::Succeeded,
            5 => GoalStatus::Canceled,
            6 => GoalStatus::Aborted,
            _ => panic!("unknown action status: {}", s),
        }
    }
}

pub struct WrappedActionClient<T>
where
    T: WrappedActionTypeSupport,
{
    pub rcl_handle: rcl_action_client_t,
    pub goal_response_channels: Vec<(
        i64,
        oneshot::Sender<
            <<T as WrappedActionTypeSupport>::SendGoal as WrappedServiceTypeSupport>::Response,
        >,
    )>,
    pub cancel_response_channels: Vec<(i64, oneshot::Sender<action_msgs::srv::CancelGoal::Response>)>,
    pub feedback_senders: Vec<(uuid::Uuid, mpsc::Sender<T::Feedback>)>,
    pub result_requests: Vec<(i64, uuid::Uuid)>,
    pub result_senders: Vec<(uuid::Uuid, oneshot::Sender<T::Result>)>,
    pub goal_status: HashMap<uuid::Uuid, GoalStatus>,
}

pub trait ActionClient_ {
    fn handle(&self) -> &rcl_action_client_t;
    fn destroy(&mut self, node: &mut rcl_node_t) -> ();

    fn handle_goal_response(&mut self) -> ();
    fn handle_cancel_response(&mut self) -> ();
    fn handle_feedback_msg(&mut self) -> ();
    fn handle_status_msg(&mut self) -> ();
    fn handle_result_response(&mut self) -> ();

    fn send_result_request(&mut self, uuid: uuid::Uuid) -> ();
}

use std::convert::TryInto;
pub fn vec_to_uuid_bytes<T>(v: Vec<T>) -> [T; 16] {
    v.try_into().unwrap_or_else(|v: Vec<T>| {
        panic!("Expected a Vec of length {} but it was {}", 16, v.len())
    })
}

impl<T> WrappedActionClient<T>
where
    T: WrappedActionTypeSupport,
{
    pub fn get_goal_status(&self, uuid: &uuid::Uuid) -> GoalStatus {
        *self.goal_status.get(uuid).unwrap_or(&GoalStatus::Unknown)
    }

    pub fn send_cancel_request(&mut self, goal: &uuid::Uuid) -> Result<impl Future<Output = Result<()>>>
    where
        T: WrappedActionTypeSupport,
    {
        let msg = action_msgs::srv::CancelGoal::Request {
            goal_info: action_msgs::msg::GoalInfo {
                goal_id: unique_identifier_msgs::msg::UUID {
                    uuid: goal.as_bytes().to_vec(),
                },
                ..action_msgs::msg::GoalInfo::default()
            },
        };
        let native_msg = WrappedNativeMsg::<action_msgs::srv::CancelGoal::Request>::from(&msg);
        let mut seq_no = 0i64;
        let result = unsafe {
            rcl_action_send_cancel_request(&self.rcl_handle, native_msg.void_ptr(), &mut seq_no)
        };

        if result == RCL_RET_OK as i32 {
            let (cancel_req_sender, cancel_req_receiver) =
                oneshot::channel::<action_msgs::srv::CancelGoal::Response>();

            self.cancel_response_channels
                .push((seq_no, cancel_req_sender));
            // instead of "canceled" we return invalid client.
            let future = cancel_req_receiver
                .map_err(|_| Error::RCL_RET_CLIENT_INVALID)
                .map(|r| match r {
                    Ok(r) => match r.return_code {
                        0 => Ok(()),
                        1 => Err(Error::GoalCancelRejected),
                        2 => Err(Error::GoalCancelUnknownGoalID),
                        3 => Err(Error::GoalCancelAlreadyTerminated),
                        x => panic!("unknown error code return from action server: {}", x),
                    },
                    Err(e) => Err(e),
                });
            Ok(future)
        } else {
            eprintln!("coult not send goal request {}", result);
            Err(Error::from_rcl_error(result))
        }
    }
}

impl<T: 'static> ActionClient_ for WrappedActionClient<T>
where
    T: WrappedActionTypeSupport,
{
    fn handle(&self) -> &rcl_action_client_t {
        &self.rcl_handle
    }

    fn handle_goal_response(&mut self) -> () {
        let mut request_id = MaybeUninit::<rmw_request_id_t>::uninit();
        let mut response_msg = WrappedNativeMsg::<
            <<T as WrappedActionTypeSupport>::SendGoal as WrappedServiceTypeSupport>::Response,
        >::new();

        let ret = unsafe {
            rcl_action_take_goal_response(
                &self.rcl_handle,
                request_id.as_mut_ptr(),
                response_msg.void_ptr_mut(),
            )
        };
        if ret == RCL_RET_OK as i32 {
            let request_id = unsafe { request_id.assume_init() };
            if let Some(idx) = self
                .goal_response_channels
                .iter()
                .position(|(id, _)| id == &request_id.sequence_number)
            {
                let (_, sender) = self.goal_response_channels.swap_remove(idx);
                let response = <<T as WrappedActionTypeSupport>::SendGoal as WrappedServiceTypeSupport>::Response::from_native(&response_msg);
                match sender.send(response) {
                    Ok(()) => {}
                    Err(e) => {
                        println!("error sending to action client: {:?}", e);
                    }
                }
            } else {
                let we_have: String = self
                    .goal_response_channels
                    .iter()
                    .map(|(id, _)| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                eprintln!(
                    "no such req id: {}, we have [{}], ignoring",
                    request_id.sequence_number, we_have
                );
            }
        }
    }

    fn handle_cancel_response(&mut self) -> () {
        let mut request_id = MaybeUninit::<rmw_request_id_t>::uninit();
        let mut response_msg = WrappedNativeMsg::<action_msgs::srv::CancelGoal::Response>::new();

        let ret = unsafe {
            rcl_action_take_cancel_response(
                &self.rcl_handle,
                request_id.as_mut_ptr(),
                response_msg.void_ptr_mut(),
            )
        };
        if ret == RCL_RET_OK as i32 {
            let request_id = unsafe { request_id.assume_init() };
            if let Some(idx) = self
                .cancel_response_channels
                .iter()
                .position(|(id, _)| id == &request_id.sequence_number)
            {
                let (_, sender) = self.cancel_response_channels.swap_remove(idx);
                let response = action_msgs::srv::CancelGoal::Response::from_native(&response_msg);
                match sender.send(response) {
                    Err(e) => eprintln!("warning: could not send cancel response msg ({:?})", e),
                    _ => (),
                }
            } else {
                let we_have: String = self
                    .goal_response_channels
                    .iter()
                    .map(|(id, _)| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                eprintln!(
                    "no such req id: {}, we have [{}], ignoring",
                    request_id.sequence_number, we_have
                );
            }
        }
    }

    fn handle_feedback_msg(&mut self) -> () {
        let mut feedback_msg = WrappedNativeMsg::<T::FeedbackMessage>::new();
        let ret =
            unsafe { rcl_action_take_feedback(&self.rcl_handle, feedback_msg.void_ptr_mut()) };
        if ret == RCL_RET_OK as i32 {
            let msg = T::FeedbackMessage::from_native(&feedback_msg);
            let (uuid, feedback) = T::destructure_feedback_msg(msg);
            let msg_uuid = uuid::Uuid::from_bytes(vec_to_uuid_bytes(uuid.uuid));
            if let Some((_, sender)) = self
                .feedback_senders
                .iter_mut()
                .find(|(uuid, _)| uuid == &msg_uuid)
            {
                match sender.try_send(feedback) {
                    Err(e) => eprintln!("warning: could not send feedback msg ({})", e),
                    _ => (),
                }
            }
        }
    }

    fn handle_status_msg(&mut self) -> () {
        let mut status_array = WrappedNativeMsg::<action_msgs::msg::GoalStatusArray>::new();
        let ret = unsafe { rcl_action_take_status(&self.rcl_handle, status_array.void_ptr_mut()) };
        if ret == RCL_RET_OK as i32 {
            let arr = action_msgs::msg::GoalStatusArray::from_native(&status_array);
            for a in &arr.status_list {
                let uuid =
                    uuid::Uuid::from_bytes(vec_to_uuid_bytes(a.goal_info.goal_id.uuid.clone()));
                if !self.result_senders.iter().any(|(suuid, _)| suuid == &uuid) {
                    continue;
                }
                let status = GoalStatus::from_rcl(a.status);
                *self.goal_status.entry(uuid).or_insert(GoalStatus::Unknown) = status;
            }
        }
    }

    fn handle_result_response(&mut self) -> () {
        let mut request_id = MaybeUninit::<rmw_request_id_t>::uninit();
        let mut response_msg = WrappedNativeMsg::<
            <<T as WrappedActionTypeSupport>::GetResult as WrappedServiceTypeSupport>::Response,
        >::new();

        let ret = unsafe {
            rcl_action_take_result_response(
                &self.rcl_handle,
                request_id.as_mut_ptr(),
                response_msg.void_ptr_mut(),
            )
        };

        if ret == RCL_RET_OK as i32 {
            let request_id = unsafe { request_id.assume_init() };
            if let Some(idx) = self
                .result_requests
                .iter()
                .position(|(id, _)| id == &request_id.sequence_number)
            {
                let (_, uuid) = self.result_requests.swap_remove(idx);
                if let Some(idx) = self
                    .result_senders
                    .iter()
                    .position(|(suuid, _)| suuid == &uuid)
                {
                    let (_, sender) = self.result_senders.swap_remove(idx);
                    let response = <<T as WrappedActionTypeSupport>::GetResult as WrappedServiceTypeSupport>::Response::from_native(&response_msg);
                    let (status, result) = T::destructure_result_response_msg(response);
                    let status = GoalStatus::from_rcl(status);
                    if status != GoalStatus::Succeeded {
                        println!("goal status failed: {:?}, result: {:?}", status, result);
                        // this will drop the sender which makes the receiver fail with "canceled"
                    } else {
                        match sender.send(result) {
                            Ok(()) => {}
                            Err(e) => {
                                println!("error sending result to action client: {:?}", e);
                            }
                        }
                    }
                }
            } else {
                let we_have: String = self
                    .result_requests
                    .iter()
                    .map(|(id, _)| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                eprintln!(
                    "no such req id: {}, we have [{}], ignoring",
                    request_id.sequence_number, we_have
                );
            }
        }
    }

    fn send_result_request(&mut self, uuid: uuid::Uuid) -> () {
        let uuid_msg = unique_identifier_msgs::msg::UUID {
            uuid: uuid.as_bytes().to_vec(),
        };
        let request_msg = T::make_result_request_msg(uuid_msg);
        let native_msg = WrappedNativeMsg::<
            <<T as WrappedActionTypeSupport>::GetResult as WrappedServiceTypeSupport>::Request,
        >::from(&request_msg);
        let mut seq_no = 0i64;
        let result = unsafe {
            rcl_action_send_result_request(&self.rcl_handle, native_msg.void_ptr(), &mut seq_no)
        };

        if result == RCL_RET_OK as i32 {
            self.result_requests.push((seq_no, uuid));
        } else {
            eprintln!("coult not send request {}", result);
        }
    }

    fn destroy(&mut self, node: &mut rcl_node_t) {
        unsafe {
            rcl_action_client_fini(&mut self.rcl_handle, node);
        }
    }
}