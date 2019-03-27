use std::cell::RefCell;
use std::rc::Rc;

use actix_router::ResourceDef;
use actix_service::boxed::BoxedNewService;
use actix_service::NewService;

use crate::dev::{HttpServiceFactory, ServiceConfig};
use crate::error::Error;
use crate::guard::Guard;
use crate::rmap::ResourceMap;
use crate::service::{ServiceFactory, ServiceRequest, ServiceResponse};

type Guards = Vec<Box<Guard>>;
type HttpNewService<P> =
    BoxedNewService<(), ServiceRequest<P>, ServiceResponse, Error, ()>;

pub struct Group<P, T = GroupEndpoint<P>> {
    endpoint: T,
    services: Vec<Box<ServiceFactory<P>>>,
    guards: Vec<Box<Guard>>,
    default: Rc<RefCell<Option<Rc<HttpNewService<P>>>>>,
    factory_ref: Rc<RefCell<Option<GroupFactory<P>>>>,
}

impl<P: 'static> Group<P> {
    /// Create a new scope
    pub fn new() -> Group<P> {
        let fref = Rc::new(RefCell::new(None));
        Group {
            endpoint: GroupEndpoint::new(fref.clone()),
            guards: Vec::new(),
            services: Vec::new(),
            default: Rc::new(RefCell::new(None)),
            factory_ref: fref,
        }
    }
}

impl<P, T> HttpServiceFactory<P> for Group<P, T>
where
    P: 'static,
    T: NewService<
            Request = ServiceRequest<P>,
            Response = ServiceResponse,
            Error = Error,
            InitError = (),
        > + 'static,
{
    fn register(self, config: &mut ServiceConfig<P>) {
        // update default resource if needed
        if self.default.borrow().is_none() {
            *self.default.borrow_mut() = Some(config.default_service());
        }

        // register nested services
        let mut cfg = config.clone_config();
        self.services
            .into_iter()
            .for_each(|mut srv| srv.register(&mut cfg));

        let mut rmap = ResourceMap::new(ResourceDef::root_prefix(&self.rdef));

        // complete scope pipeline creation
        *self.factory_ref.borrow_mut() = Some(GroupFactory {
            default: self.default.clone(),
            services: Rc::new(
                cfg.into_services()
                    .into_iter()
                    .map(|(mut rdef, srv, guards, nested)| {
                        rmap.add(&mut rdef, nested);
                        (rdef, srv, RefCell::new(guards))
                    })
                    .collect(),
            ),
        });

        // get guards
        let guards = if self.guards.is_empty() {
            None
        } else {
            Some(self.guards)
        };

        // register final service
        config.register_service(
            ResourceDef::root_prefix(&self.rdef),
            guards,
            self.endpoint,
            Some(Rc::new(rmap)),
        )
    }
}

pub struct GroupFactory<P> {
    services: Rc<Vec<(ResourceDef, HttpNewService<P>, RefCell<Option<Guards>>)>>,
    default: Rc<RefCell<Option<Rc<HttpNewService<P>>>>>,
}

pub struct GroupEndpoint<P> {
    factory: Rc<RefCell<Option<GroupFactory<P>>>>,
}

impl<P> GroupEndpoint<P> {
    fn new(factory: Rc<RefCell<Option<GroupFactory<P>>>>) -> Self {
        GroupEndpoint { factory }
    }
}
