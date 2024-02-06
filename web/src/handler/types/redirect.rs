//! type responder for http redirecting response.

use core::convert::Infallible;

use xitca_http::util::service::router::{RouterGen, RouterMapErr};

use crate::{
    body::ResponseBody,
    error::{Error, ErrorStatus},
    handler::Responder,
    http::{
        header::{HeaderValue, LOCATION},
        StatusCode, WebResponse,
    },
    service::Service,
    WebContext,
};

#[derive(Clone)]
pub struct Redirect {
    status: StatusCode,
    location: Result<HeaderValue, ErrorStatus>,
}

macro_rules! variants {
    ($name: tt, $status: tt) => {
        #[inline]
        pub fn $name(uri: impl TryInto<HeaderValue>) -> Self {
            Self::new(StatusCode::$status, uri)
        }
    };
}

impl Redirect {
    variants!(found, FOUND);
    variants!(see_other, SEE_OTHER);
    variants!(temporary, TEMPORARY_REDIRECT);
    variants!(permanent, PERMANENT_REDIRECT);

    fn new(status: StatusCode, uri: impl TryInto<HeaderValue>) -> Self {
        Self {
            status,
            location: uri.try_into().map_err(|_| ErrorStatus::internal()),
        }
    }
}

impl<'r, C, B> Responder<WebContext<'r, C, B>> for Redirect {
    type Response = WebResponse;
    type Error = Error<C>;

    async fn respond(self, ctx: WebContext<'r, C, B>) -> Result<Self::Response, Self::Error> {
        let res = ctx.into_response(ResponseBody::empty());
        Responder::<WebContext<'r, C, B>>::map(self, res)
    }

    fn map(self, res: Self::Response) -> Result<Self::Response, Self::Error> {
        let location = self.location.map_err(|_| ErrorStatus::internal())?;
        let map = (self.status, (LOCATION, location));
        Responder::<WebContext<'r, C, B>>::map(map, res)
    }
}

impl RouterGen for Redirect {
    type Route<R> = RouterMapErr<R>;

    fn route_gen<R>(route: R) -> Self::Route<R> {
        RouterMapErr(route)
    }
}

impl Service for Redirect {
    type Response = Self;
    type Error = Infallible;

    async fn call(&self, _: ()) -> Result<Self::Response, Self::Error> {
        Ok(self.clone())
    }
}

impl<'r, C, B> Service<WebContext<'r, C, B>> for Redirect {
    type Response = WebResponse;
    type Error = Error<C>;

    #[inline]
    async fn call(&self, ctx: WebContext<'r, C, B>) -> Result<Self::Response, Self::Error> {
        self.clone().respond(ctx).await
    }
}

#[cfg(test)]
mod test {
    use xitca_unsafe_collection::futures::NowOrPanic;

    use crate::{http::WebRequest, App};

    use super::*;

    #[test]
    fn respond() {
        let redirect = Redirect::see_other("/996");

        let mut ctx = WebContext::new_test(&());
        let ctx = ctx.as_web_ctx();

        let res = redirect.respond(ctx).now_or_panic().unwrap();
        assert_eq!(res.status().as_u16(), 303);
        assert_eq!(res.headers().get(LOCATION).unwrap().to_str().unwrap(), "/996")
    }

    #[test]
    fn service() {
        let res = App::new()
            .at("/", Redirect::see_other("/996"))
            .finish()
            .call(())
            .now_or_panic()
            .unwrap()
            .call(WebRequest::default())
            .now_or_panic()
            .unwrap();
        assert_eq!(res.status().as_u16(), 303);
        assert_eq!(res.headers().get(LOCATION).unwrap().to_str().unwrap(), "/996")
    }
}
