use std::cell::RefCell;
use std::rc::Rc;

use actix_http::Response;
use actix_router::{ResourceDef, ResourceInfo, Router};
use actix_service::boxed::{self, BoxedNewService, BoxedService};
use actix_service::{
    ApplyTransform, IntoNewService, IntoTransform, NewService, Service, Transform,
};
use futures::future::{ok, Either, Future, FutureResult};
use futures::{Async, IntoFuture, Poll};

use crate::dev::{HttpServiceFactory, ServiceConfig};
use crate::error::Error;
use crate::guard::Guard;
use crate::http::{header, HeaderValue};
use crate::resource::Resource;
use crate::rmap::ResourceMap;
use crate::route::Route;
use crate::service::{
    ServiceFactory, ServiceFactoryWrapper, ServiceRequest, ServiceResponse,
};
use crate::web;

type Guards = Vec<Box<Guard>>;
type HttpService<P> = BoxedService<ServiceRequest<P>, ServiceResponse, Error>;
type HttpNewService<P> =
    BoxedNewService<(), ServiceRequest<P>, ServiceResponse, Error, ()>;
type BoxedResponse = Box<Future<Item = ServiceResponse, Error = Error>>;
type Middleware<M: Transform<T::Service>, T: NewService> = IntoTransform<M, T::Service>;

pub struct Group<P, M, T: NewService> {
    // endpoint: T,
    services: Vec<Box<ServiceFactory<P>>>,
    guards: Vec<Box<Guard>>,
    middlewares: Vec<Rc<Middleware<M, T>>>, // default: Rc<RefCell<Option<Rc<HttpNewService<P>>>>>,
                                            // factory_ref: Rc<RefCell<Option<GroupFactory<P>>>>
}

impl<P: 'static, M, T: NewService> Group<P, M, T> {
    /// Create a new scope
    pub fn new() -> Group<P, M, T> {
        // let fref = Rc::new(RefCell::new(None));
        Group {
            // endpoint: GroupEndpoint::new(fref.clone()),
            guards: Vec::new(),
            services: Vec::new(),
            middlewares: Vec::new()
            // default: Rc::new(RefCell::new(None)),
            // factory_ref: fref,
        }
    }
}

impl<P, M, T> Group<P, M, T>
where
    P: 'static,
    T: NewService<
        Request = ServiceRequest<P>,
        Response = ServiceResponse,
        Error = Error,
        InitError = (),
    >,
    M: Transform<
        T::Service,
        Request = ServiceRequest<P>,
        Response = ServiceResponse,
        Error = Error,
        InitError = (),
    >,
{
    /// Add match guard to a scope.
    ///
    /// ```rust
    /// use actix_web::{web, guard, App, HttpRequest, HttpResponse};
    ///
    /// fn index(data: web::Path<(String, String)>) -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::scope("/app")
    ///             .guard(guard::Header("content-type", "text/plain"))
    ///             .route("/test1", web::get().to(index))
    ///             .route("/test2", web::post().to(|r: HttpRequest| {
    ///                 HttpResponse::MethodNotAllowed()
    ///             }))
    ///     );
    /// }
    /// ```
    pub fn guard<G: Guard + 'static>(mut self, guard: G) -> Self {
        self.guards.push(Box::new(guard));
        self
    }

    /// Register http service.
    ///
    /// This is similar to `App's` service registration.
    ///
    /// Actix web provides several services implementations:
    ///
    /// * *Resource* is an entry in resource table which corresponds to requested URL.
    /// * *Scope* is a set of resources with common root path.
    /// * "StaticFiles" is a service for static files support
    ///
    /// ```rust
    /// use actix_web::{web, App, HttpRequest};
    ///
    /// struct AppState;
    ///
    /// fn index(req: HttpRequest) -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::scope("/app").service(
    ///             web::scope("/v1")
    ///                 .service(web::resource("/test1").to(index)))
    ///     );
    /// }
    /// ```
    pub fn service<F>(mut self, factory: F) -> Self
    where
        F: HttpServiceFactory<P> + 'static,
    {
        self.services
            .push(Box::new(ServiceFactoryWrapper::new(factory)));
        self
    }

    /// Configure route for a specific path.
    ///
    /// This is a simplified version of the `Scope::service()` method.
    /// This method can be called multiple times, in that case
    /// multiple resources with one route would be registered for same resource path.
    ///
    /// ```rust
    /// use actix_web::{web, App, HttpResponse};
    ///
    /// fn index(data: web::Path<(String, String)>) -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::scope("/app")
    ///             .route("/test1", web::get().to(index))
    ///             .route("/test2", web::post().to(|| HttpResponse::MethodNotAllowed()))
    ///     );
    /// }
    /// ```
    pub fn route(self, path: &str, mut route: Route<P>) -> Self {
        self.service(
            Resource::new(path)
                .add_guards(route.take_guards())
                .route(route),
        )
    }

    /// Default resource to be used if no matching route could be found.
    ///
    /// If default resource is not registered, app's default resource is being used.
    // pub fn default_resource<F, U>(mut self, f: F) -> Self
    // where
    //     F: FnOnce(Resource<P>) -> Resource<P, U>,
    //     U: NewService<
    //             Request = ServiceRequest<P>,
    //             Response = ServiceResponse,
    //             Error = Error,
    //             InitError = (),
    //         > + 'static,
    // {
    //     // create and configure default resource
    //     self.default = Rc::new(RefCell::new(Some(Rc::new(boxed::new_service(
    //         f(Resource::new("")).into_new_service().map_init_err(|_| ()),
    //     )))));

    //     self
    // }

    /// Register a group level middleware.
    ///
    /// This is similar to `App's` middlewares, but middleware get invoked on group level.
    /// Group level middlewares are not allowed to change response
    /// type (i.e modify response's body).
    pub fn wrap<F>(self, mw: F) -> Self
    where
        F: IntoTransform<M, T::Service> + 'static,
    {
        self.middlewares.push(Rc::new(mw));

        self
    }

    /// Register a scope level middleware function.
    ///
    /// This function accepts instance of `ServiceRequest` type and
    /// mutable reference to the next middleware in chain.
    ///
    /// ```rust
    /// use actix_service::Service;
    /// # use futures::Future;
    /// use actix_web::{web, App};
    /// use actix_web::http::{header::CONTENT_TYPE, HeaderValue};
    ///
    /// fn index() -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::scope("/app")
    ///             .wrap_fn(|req, srv|
    ///                 srv.call(req).map(|mut res| {
    ///                     res.headers_mut().insert(
    ///                        CONTENT_TYPE, HeaderValue::from_static("text/plain"),
    ///                     );
    ///                     res
    ///                 }))
    ///             .route("/index.html", web::get().to(index)));
    /// }
    /// ```
    pub fn wrap_fn<F, R>(self, mw: F) -> Self
    where
        F: FnMut(ServiceRequest<P>, &mut T::Service) -> R + Clone,
        R: IntoFuture<Item = ServiceResponse, Error = Error>,
    {
        self.wrap(mw)
    }
}

impl<P, M, T> HttpServiceFactory<P> for Group<P, M, T>
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
        // if self.default.borrow().is_none() {
        //     *self.default.borrow_mut() = Some(config.default_service());
        // }

        // register nested services
        let mut cfg = config.clone_config();
        self.services
            .into_iter()
            .for_each(|mut srv| srv.register(&mut cfg));

        cfg.into_services()
            .into_iter()
            .for_each(|(rdef, srv, guards, nested)| {
                //apply middlewares..
                for mid in self.middlewares {
                    ApplyTransform::new(mid, srv);
                }
            });

        // let mut rmap = ResourceMap::new(ResourceDef::root_prefix(&self.rdef));

        // complete group pipeline creation
        // *self.factory_ref.borrow_mut() = Some(GroupFactory {
        //     default: self.default.clone(),
        //     services: Rc::new(
        //         cfg.into_services()
        //             .into_iter()
        //             .map(|(mut rdef, mut srv, guards, nested)| {
        //                 // rmap.add(&mut rdef, nested);
        //                 for e in 1..5 {
        //                     srv = boxed::new_service(ApplyTransform::new(md, srv));
        //                 }

        //                 config.register_service(rdef.clone(), guards, srv, nested);

        //                 (rdef, srv, RefCell::new(guards))
        //             })
        //             .collect(),
        //     ),
        // });

        // get guards
        // let guards = if self.guards.is_empty() {
        //     None
        // } else {
        //     Some(self.guards)
        // };

        // register final service
        // config.register_service(
        //     ResourceDef::root_prefix(&self.rdef),
        //     guards,
        //     self.endpoint,
        //     Some(Rc::new(rmap)),
        // )
    }
}

fn md<S, P, B>(
    req: ServiceRequest<P>,
    srv: &mut S,
) -> impl IntoFuture<Item = ServiceResponse<B>, Error = Error>
where
    S: Service<
        Request = ServiceRequest<P>,
        Response = ServiceResponse<B>,
        Error = Error,
    >,
{
    srv.call(req).map(|mut res| {
        res.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("0001"));
        res
    })
}

// pub struct GroupFactory<P> {
//     services: Rc<Vec<(ResourceDef, HttpNewService<P>, RefCell<Option<Guards>>)>>,
//     default: Rc<RefCell<Option<Rc<HttpNewService<P>>>>>,
// }

// impl<P: 'static> NewService for GroupFactory<P> {
//     type Request = ServiceRequest<P>;
//     type Response = ServiceResponse;
//     type Error = Error;
//     type InitError = ();
//     type Service = GroupService<P>;
//     type Future = GroupFactoryResponse<P>;

//     fn new_service(&self, _: &()) -> Self::Future {
//         let default_fut = if let Some(ref default) = *self.default.borrow() {
//             Some(default.new_service(&()))
//         } else {
//             None
//         };

//         GroupFactoryResponse {
//             fut: self
//                 .services
//                 .iter()
//                 .map(|(path, service, guards)| {
//                     CreateGroupServiceItem::Future(
//                         Some(path.clone()),
//                         guards.borrow_mut().take(),
//                         service.new_service(&()),
//                     )
//                 })
//                 .collect(),
//             default: None,
//             default_fut,
//         }
//     }
// }

// /// Create scope service
// #[doc(hidden)]
// pub struct GroupFactoryResponse<P> {
//     fut: Vec<CreateGroupServiceItem<P>>,
//     default: Option<HttpService<P>>,
//     default_fut: Option<Box<Future<Item = HttpService<P>, Error = ()>>>,
// }

// type HttpServiceFut<P> = Box<Future<Item = HttpService<P>, Error = ()>>;

// enum CreateGroupServiceItem<P> {
//     Future(Option<ResourceDef>, Option<Guards>, HttpServiceFut<P>),
//     Service(ResourceDef, Option<Guards>, HttpService<P>),
// }

// impl<P> Future for GroupFactoryResponse<P> {
//     type Item = GroupService<P>;
//     type Error = ();

//     fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
//         let mut done = true;

//         if let Some(ref mut fut) = self.default_fut {
//             match fut.poll()? {
//                 Async::Ready(default) => self.default = Some(default),
//                 Async::NotReady => done = false,
//             }
//         }

//         // poll http services
//         for item in &mut self.fut {
//             let res = match item {
//                 CreateGroupServiceItem::Future(
//                     ref mut path,
//                     ref mut guards,
//                     ref mut fut,
//                 ) => match fut.poll()? {
//                     Async::Ready(service) => {
//                         Some((path.take().unwrap(), guards.take(), service))
//                     }
//                     Async::NotReady => {
//                         done = false;
//                         None
//                     }
//                 },
//                 CreateGroupServiceItem::Service(_, _, _) => continue,
//             };

//             if let Some((path, guards, service)) = res {
//                 *item = CreateGroupServiceItem::Service(path, guards, service);
//             }
//         }

//         if done {
//             let router = self
//                 .fut
//                 .drain(..)
//                 .fold(Router::build(), |mut router, item| {
//                     match item {
//                         CreateGroupServiceItem::Service(path, guards, service) => {
//                             router.rdef(path, service).2 = guards;
//                         }
//                         CreateGroupServiceItem::Future(_, _, _) => unreachable!(),
//                     }
//                     router
//                 });
//             Ok(Async::Ready(GroupService {
//                 router: router.finish(),
//                 default: self.default.take(),
//                 _ready: None,
//             }))
//         } else {
//             Ok(Async::NotReady)
//         }
//     }
// }

// pub struct GroupService<P> {
//     router: Router<HttpService<P>, Vec<Box<Guard>>>,
//     default: Option<HttpService<P>>,
//     _ready: Option<(ServiceRequest<P>, ResourceInfo)>,
// }

// impl<P> Service for GroupService<P> {
//     type Request = ServiceRequest<P>;
//     type Response = ServiceResponse;
//     type Error = Error;
//     type Future = Either<BoxedResponse, FutureResult<Self::Response, Self::Error>>;

//     fn poll_ready(&mut self) -> Poll<(), Self::Error> {
//         Ok(Async::Ready(()))
//     }

//     fn call(&mut self, mut req: ServiceRequest<P>) -> Self::Future {
//         let res = self.router.recognize_mut_checked(&mut req, |req, guards| {
//             if let Some(ref guards) = guards {
//                 for f in guards {
//                     if !f.check(req.head()) {
//                         return false;
//                     }
//                 }
//             }
//             true
//         });

//         if let Some((srv, _info)) = res {
//             Either::A(srv.call(req))
//         } else if let Some(ref mut default) = self.default {
//             Either::A(default.call(req))
//         } else {
//             let req = req.into_parts().0;
//             Either::B(ok(ServiceResponse::new(req, Response::NotFound().finish())))
//         }
//     }
// }

// pub struct GroupEndpoint<P> {
//     factory: Rc<RefCell<Option<GroupFactory<P>>>>,
// }

// impl<P> GroupEndpoint<P> {
//     fn new(factory: Rc<RefCell<Option<GroupFactory<P>>>>) -> Self {
//         GroupEndpoint { factory }
//     }
// }

// impl<P: 'static> NewService for GroupEndpoint<P> {
//     type Request = ServiceRequest<P>;
//     type Response = ServiceResponse;
//     type Error = Error;
//     type InitError = ();
//     type Service = GroupService<P>;
//     type Future = GroupFactoryResponse<P>;

//     fn new_service(&self, _: &()) -> Self::Future {
//         self.factory.borrow_mut().as_mut().unwrap().new_service(&())
//     }
// }
