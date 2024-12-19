use std::cell::Cell;
use std::cell::OnceCell;
use std::cell::RefCell;
use std::sync::OnceLock;

use gio::prelude::*;
use gio::subclass::prelude::*;
use glib::clone;
use glib::subclass::Signal;
use glib::Properties;
use gtk::gio;
use gtk::glib;
use indexmap::map::Entry;
use indexmap::map::IndexMap;

use crate::model;
use crate::model::SelectableListExt;
use crate::podman;
use crate::utils;

mod imp {
    use super::*;

    #[derive(Debug, Default, Properties)]
    #[properties(wrapper_type = super::PodList)]
    pub(crate) struct PodList {
        pub(super) list: RefCell<IndexMap<String, model::Pod>>,
        #[property(get, set, construct_only, nullable)]
        pub(super) client: glib::WeakRef<model::Client>,
        #[property(get)]
        pub(super) listing: Cell<bool>,
        #[property(get = Self::is_initialized, type = bool)]
        pub(super) initialized: OnceCell<()>,
        #[property(get, set)]
        pub(super) selection_mode: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PodList {
        const NAME: &'static str = "PodList";
        type Type = super::PodList;
        type Interfaces = (gio::ListModel, model::SelectableList);
    }

    impl ObjectImpl for PodList {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("pod-added")
                        .param_types([model::Pod::static_type()])
                        .build(),
                    Signal::builder("containers-in-pod-changed")
                        .param_types([model::Pod::static_type()])
                        .build(),
                ]
            })
        }

        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
            PROPERTIES.get_or_init(|| {
                Self::derived_properties()
                    .iter()
                    .cloned()
                    .chain(vec![
                        glib::ParamSpecUInt::builder("len").read_only().build(),
                        glib::ParamSpecUInt::builder("degraded").read_only().build(),
                        glib::ParamSpecUInt::builder("not-running")
                            .read_only()
                            .build(),
                        glib::ParamSpecUInt::builder("paused").read_only().build(),
                        glib::ParamSpecUInt::builder("running").read_only().build(),
                        glib::ParamSpecUInt::builder("num-selected")
                            .read_only()
                            .build(),
                    ])
                    .collect::<Vec<_>>()
            })
        }

        fn set_property(&self, id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            self.derived_set_property(id, value, pspec);
        }

        fn property(&self, id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "len" => self.obj().len().to_value(),
                "degraded" => self.obj().degraded().to_value(),
                "not-running" => self.obj().not_running().to_value(),
                "paused" => self.obj().paused().to_value(),
                "running" => self.obj().running().to_value(),
                "num-selected" => self.obj().num_selected().to_value(),
                _ => self.derived_property(id, pspec),
            }
        }
        fn constructed(&self) {
            self.parent_constructed();
            let obj = &*self.obj();
            model::SelectableList::bootstrap(obj.upcast_ref());
            obj.connect_items_changed(|self_, _, _, _| self_.notify("len"));

            obj.connect_pod_added(|list, pod| {
                pod.connect_num_containers_notify(clone!(
                    #[weak]
                    list,
                    move |pod| list.emit_by_name::<()>("containers-in-pod-changed", &[pod])
                ));
            });
        }
    }

    impl ListModelImpl for PodList {
        fn item_type(&self) -> glib::Type {
            model::Pod::static_type()
        }

        fn n_items(&self) -> u32 {
            self.list.borrow().len() as u32
        }

        fn item(&self, position: u32) -> Option<glib::Object> {
            self.list
                .borrow()
                .get_index(position as usize)
                .map(|(_, obj)| obj.upcast_ref())
                .cloned()
        }
    }

    impl PodList {
        pub(super) fn is_initialized(&self) -> bool {
            self.initialized.get().is_some()
        }

        pub(super) fn set_as_initialized(&self) {
            if self.is_initialized() {
                return;
            }
            self.initialized.set(()).unwrap();
            self.obj().notify_initialized();
        }

        pub(super) fn set_listing(&self, value: bool) {
            let obj = &*self.obj();
            if obj.listing() == value {
                return;
            }
            self.listing.set(value);
            obj.notify_listing();
        }
    }
}

glib::wrapper! {
    pub(crate) struct PodList(ObjectSubclass<imp::PodList>)
        @implements gio::ListModel, model::SelectableList;
}

impl From<&model::Client> for PodList {
    fn from(client: &model::Client) -> Self {
        glib::Object::builder().property("client", client).build()
    }
}

impl PodList {
    fn notify_num_pods(&self) {
        self.notify("degraded");
        self.notify("not-running");
        self.notify("paused");
        self.notify("running");
    }

    pub(crate) fn len(&self) -> u32 {
        self.n_items()
    }

    pub(crate) fn degraded(&self) -> u32 {
        self.num_pods_of_status(model::PodStatus::Degraded)
    }

    pub(crate) fn not_running(&self) -> u32 {
        self.len() - self.running() - self.paused() - self.degraded()
    }

    pub(crate) fn paused(&self) -> u32 {
        self.num_pods_of_status(model::PodStatus::Paused)
    }

    pub(crate) fn running(&self) -> u32 {
        self.num_pods_of_status(model::PodStatus::Running)
    }

    pub(crate) fn num_pods_of_status(&self, status: model::PodStatus) -> u32 {
        self.imp()
            .list
            .borrow()
            .values()
            .filter(|pod| pod.status() == status)
            .count() as u32
    }

    pub(crate) fn get_pod(&self, id: &str) -> Option<model::Pod> {
        self.imp().list.borrow().get(id).cloned()
    }

    pub(crate) fn remove_pod(&self, id: &str) {
        let mut list = self.imp().list.borrow_mut();
        if let Some((idx, _, pod)) = list.shift_remove_full(id) {
            drop(list);

            self.items_changed(idx as u32, 1, 0);
            self.notify_num_pods();
            pod.emit_deleted();
        }
    }

    pub(crate) fn refresh<F>(&self, id: Option<String>, err_op: F)
    where
        F: FnOnce(super::RefreshError) + Clone + 'static,
    {
        self.imp().set_listing(true);
        utils::do_async(
            {
                let podman = self.client().unwrap().podman();
                let id = id.clone();
                async move {
                    podman
                        .pods()
                        .list(
                            &podman::opts::PodListOpts::builder()
                                .filter(
                                    id.map(podman::Id::from)
                                        .map(podman::opts::PodListFilter::Id),
                                )
                                .build(),
                        )
                        .await
                }
            },
            clone!(
                #[weak(rename_to = obj)]
                self,
                move |result| {
                    match result {
                        Ok(list_pods) => {
                            if id.is_none() {
                                let to_remove = obj
                                    .imp()
                                    .list
                                    .borrow()
                                    .keys()
                                    .filter(|id| {
                                        !list_pods
                                            .iter()
                                            .any(|list_pod| list_pod.id.as_ref() == Some(id))
                                    })
                                    .cloned()
                                    .collect::<Vec<_>>();
                                to_remove.iter().for_each(|id| {
                                    obj.remove_pod(id);
                                });
                            }

                            list_pods.into_iter().for_each(|report| {
                                let index = obj.len();

                                let mut list = obj.imp().list.borrow_mut();

                                match list.entry(report.id.as_ref().unwrap().to_owned()) {
                                    Entry::Vacant(e) => {
                                        let pod = model::Pod::new(&obj, report);
                                        e.insert(pod.clone());

                                        drop(list);

                                        obj.items_changed(index, 0, 1);
                                        obj.pod_added(&pod);
                                    }
                                    Entry::Occupied(e) => {
                                        let pod = e.get().clone();
                                        drop(list);
                                        pod.update(report);
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            log::error!("Error on retrieving pods: {}", e);
                            err_op(super::RefreshError);
                        }
                    }
                    let imp = obj.imp();
                    imp.set_listing(false);
                    imp.set_as_initialized();
                }
            ),
        );
    }

    pub(crate) fn handle_event<F>(&self, event: podman::models::Event, err_op: F)
    where
        F: FnOnce(super::RefreshError) + Clone + 'static,
    {
        let pod_id = event.actor.id;

        match event.action.as_str() {
            "remove" => self.remove_pod(&pod_id),
            _ => self.refresh(self.get_pod(&pod_id).map(|_| pod_id), err_op),
        }
    }

    fn pod_added(&self, pod: &model::Pod) {
        self.notify_num_pods();
        pod.connect_notify_local(
            Some("status"),
            clone!(
                #[weak(rename_to = obj)]
                self,
                move |_, _| obj.notify_num_pods()
            ),
        );
        self.emit_by_name::<()>("pod-added", &[pod]);
    }

    pub(crate) fn connect_pod_added<F: Fn(&Self, &model::Pod) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_signal("pod-added", f)
    }

    pub(crate) fn connect_containers_in_pod_changed<F: Fn(&Self, &model::Pod) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_signal("containers-in-pod-changed", f)
    }

    fn connect_signal<F: Fn(&Self, &model::Pod) + 'static>(
        &self,
        signal: &str,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local(signal, true, move |values| {
            let obj = values[0].get::<Self>().unwrap();
            let pod = values[1].get::<model::Pod>().unwrap();
            f(&obj, &pod);

            None
        })
    }
}
